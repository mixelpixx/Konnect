//! Native, dependency-gated Vello schematic viewer.

use crate::change_timeline::{ChangeKind, ChangeOrigin, ChangeTimeline};
use crate::edit_session::EditSession;
use crate::editor_history::{HistoryCommand, HistoryEntry};
use crate::editor_model::{
    box_selects_bounds, drag_delta_mm, kicad_lock_path, notification_matches_revision, snap_point,
    summarize_external_change,
};
use crate::native_scene::{
    connected_wire_moves, discover_hierarchy, duplicate_symbol_block, load_hierarchy,
    move_items_with_connected_wires, rotate_item_source, Bounds, ColorRole, ConnectedWireMove,
    ConnectivityDiagnosticKind, HierarchyEntry, HierarchyScene, ObjectKind, Point as SchPoint,
    Primitive, SchematicScene, TextAlign,
};
use crate::vello_render::{
    encode_primitives, encode_scene, encode_scene_without_ranges, polyline_path, round_stroke,
};
#[cfg(test)]
use crate::vello_render::{kicad_svg_arc, svg_arc_path};
use crate::viewer_settings::ViewerSettings;
use anyhow::{anyhow, Context, Result};
use fontdb::{Database, Family, Query};
use konnect_sexp::schematic::{BusEntryDirection, HierarchicalSheetSpec, SheetPinType};
use konnect_sexp::{
    commit_file_transaction, parse_sexp, prepare_command, read_consistent,
    recover_file_transactions, DocumentRevision, ItemAnchor, ItemId, SchematicCommand, SexpError,
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rustybuzz::{shape, Face as ShapingFace, UnicodeBuffer};
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};
use vello::kurbo::{
    Affine, Arc as KurboArc, BezPath, Circle, Line, Point as KurboPoint, Rect, RoundedRect, Stroke,
};
use vello::peniko::{Blob, Color, Fill, FontData};
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::{AaConfig, Glyph, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Icon, Window, WindowAttributes, WindowId};

const STATUS_HEIGHT: f64 = 40.0;
const SIDEBAR_WIDTH: f64 = 96.0;
const FILMSTRIP_HEIGHT: f64 = 180.0;
const TIMELINE_HEIGHT: f64 = 50.0;
const PAGE_PADDING: f64 = 16.0;
const THUMBNAIL_WIDTH: f64 = 150.0;
const THUMBNAIL_GAP: f64 = 8.0;
const HISTORY_LIMIT: usize = 200;
const DIAGNOSTICS_PANEL_WIDTH: f64 = 620.0;
const DIAGNOSTICS_HEADER_HEIGHT: f64 = 38.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct ScreenRect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl ScreenRect {
    fn width(self) -> f64 {
        self.x1 - self.x0
    }

    fn height(self) -> f64 {
        self.y1 - self.y0
    }

    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }

    fn center(self) -> (f64, f64) {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }

    fn as_kurbo(self) -> Rect {
        Rect::new(self.x0, self.y0, self.x1, self.y1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Theme {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) app: Color,
    pub(crate) toolbar: Color,
    pub(crate) filmstrip: Color,
    pub(crate) page: Color,
    pub(crate) fill: Color,
    pub(crate) card: Color,
    pub(crate) card_border: Color,
    pub(crate) accent: Color,
    pub(crate) border: Color,
    pub(crate) bus: Color,
    pub(crate) junction: Color,
    pub(crate) label: Color,
    pub(crate) no_connect: Color,
    pub(crate) pin: Color,
    pub(crate) sheet_file: Color,
    pub(crate) symbol: Color,
    pub(crate) text: Color,
    pub(crate) wire: Color,
    pub(crate) selection: Color,
}

impl Theme {
    fn palette(self) -> Palette {
        match self {
            Self::Dark => Palette {
                // Dark mode is a purpose-built high-contrast palette rather
                // than KiCad's light-canvas colors placed on a dark page.
                // Every semantic foreground clears 4.5:1 against `page`;
                // text clears 12:1 against all UI surfaces.
                app: rgb(18, 22, 29),
                toolbar: rgb(24, 29, 38),
                filmstrip: rgb(15, 19, 26),
                page: rgb(31, 35, 42),
                fill: rgb(49, 55, 66),
                card: rgb(36, 42, 52),
                card_border: rgb(103, 116, 137),
                accent: rgb(125, 211, 252),
                border: rgb(184, 194, 209),
                bus: rgb(126, 164, 255),
                junction: rgb(113, 218, 151),
                label: rgb(238, 242, 248),
                no_connect: rgb(192, 167, 255),
                pin: rgb(255, 172, 153),
                sheet_file: rgb(255, 209, 102),
                symbol: rgb(255, 143, 143),
                text: rgb(242, 244, 248),
                wire: rgb(113, 218, 151),
                selection: rgb(255, 202, 92),
            },
            Self::Light => Palette {
                app: rgb(226, 231, 242),
                toolbar: rgb(245, 247, 252),
                filmstrip: rgb(235, 239, 247),
                page: rgb(245, 244, 239),
                fill: rgb(255, 255, 194),
                card: rgb(250, 251, 254),
                card_border: rgb(153, 164, 190),
                accent: rgb(190, 24, 72),
                border: rgb(132, 0, 0),
                bus: rgb(0, 0, 194),
                junction: rgb(0, 150, 0),
                label: rgb(15, 15, 15),
                no_connect: rgb(0, 0, 132),
                pin: rgb(169, 0, 0),
                sheet_file: rgb(114, 86, 0),
                symbol: rgb(132, 0, 0),
                text: rgb(0, 100, 100),
                wire: rgb(0, 150, 0),
                selection: rgb(215, 28, 84),
            },
        }
    }
}

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::from_rgb8(red, green, blue)
}

fn draw_ui_icon(scene: &mut Scene, icon: UiIcon, rect: ScreenRect, color: Color) {
    let (cx, cy) = rect.center();
    let radius = rect.width().min(rect.height()) * 0.27;
    let stroke = Stroke::new(1.8);
    let mut path = BezPath::new();
    match icon {
        UiIcon::Add => {
            path.move_to((cx - radius, cy));
            path.line_to((cx + radius, cy));
            path.move_to((cx, cy - radius));
            path.line_to((cx, cy + radius));
        }
        UiIcon::Commit => {
            path.move_to((cx - radius, cy));
            path.line_to((cx - radius * 0.25, cy + radius * 0.72));
            path.line_to((cx + radius, cy - radius * 0.72));
        }
        UiIcon::Delete => {
            path.move_to((cx - radius * 0.72, cy - radius * 0.55));
            path.line_to((cx - radius * 0.5, cy + radius));
            path.line_to((cx + radius * 0.5, cy + radius));
            path.line_to((cx + radius * 0.72, cy - radius * 0.55));
            path.move_to((cx - radius, cy - radius * 0.72));
            path.line_to((cx + radius, cy - radius * 0.72));
            path.move_to((cx - radius * 0.36, cy - radius));
            path.line_to((cx + radius * 0.36, cy - radius));
        }
        UiIcon::Discard => {
            path.move_to((cx - radius * 0.75, cy - radius * 0.75));
            path.line_to((cx + radius * 0.75, cy + radius * 0.75));
            path.move_to((cx + radius * 0.75, cy - radius * 0.75));
            path.line_to((cx - radius * 0.75, cy + radius * 0.75));
        }
        UiIcon::Duplicate => {
            path.move_to((cx - radius, cy - radius));
            path.line_to((cx + radius * 0.45, cy - radius));
            path.line_to((cx + radius * 0.45, cy + radius * 0.45));
            path.line_to((cx - radius, cy + radius * 0.45));
            path.close_path();
            path.move_to((cx - radius * 0.45, cy - radius * 0.45));
            path.line_to((cx + radius, cy - radius * 0.45));
            path.line_to((cx + radius, cy + radius));
            path.line_to((cx - radius * 0.45, cy + radius));
        }
        UiIcon::Edit => {
            path.move_to((cx - radius * 0.85, cy + radius * 0.85));
            path.line_to((cx - radius * 0.55, cy + radius * 0.15));
            path.line_to((cx + radius * 0.55, cy - radius * 0.95));
            path.line_to((cx + radius * 0.95, cy - radius * 0.55));
            path.line_to((cx - radius * 0.15, cy + radius * 0.55));
            path.close_path();
        }
        UiIcon::External | UiIcon::Transform => {
            scene.stroke(
                &stroke,
                Affine::IDENTITY,
                color,
                None,
                &Circle::new((cx, cy), radius * 0.75),
            );
            path.move_to((cx + radius * 0.22, cy - radius * 0.92));
            path.line_to((cx + radius * 0.85, cy - radius * 0.8));
            path.line_to((cx + radius * 0.68, cy - radius * 0.2));
            if icon == UiIcon::External {
                path.move_to((cx - radius, cy));
                path.line_to((cx + radius, cy));
                path.move_to((cx, cy - radius));
                path.line_to((cx, cy + radius));
            }
        }
        UiIcon::Fit => {
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                path.move_to((cx + sx * radius, cy + sy * radius * 0.45));
                path.line_to((cx + sx * radius, cy + sy * radius));
                path.line_to((cx + sx * radius * 0.45, cy + sy * radius));
            }
        }
        UiIcon::Follow => {
            scene.stroke(
                &stroke,
                Affine::IDENTITY,
                color,
                None,
                &Circle::new((cx, cy), radius * 0.62),
            );
            for (x0, y0, x1, y1) in [
                (cx - radius, cy, cx - radius * 0.45, cy),
                (cx + radius * 0.45, cy, cx + radius, cy),
                (cx, cy - radius, cx, cy - radius * 0.45),
                (cx, cy + radius * 0.45, cx, cy + radius),
            ] {
                path.move_to((x0, y0));
                path.line_to((x1, y1));
            }
        }
        UiIcon::Grid => {
            for offset in [-0.62, 0.0, 0.62] {
                path.move_to((cx - radius, cy + offset * radius));
                path.line_to((cx + radius, cy + offset * radius));
                path.move_to((cx + offset * radius, cy - radius));
                path.line_to((cx + offset * radius, cy + radius));
            }
        }
        UiIcon::Highlight => {
            path.move_to((cx - radius, cy));
            path.curve_to(
                (cx - radius * 0.5, cy - radius * 0.8),
                (cx + radius * 0.5, cy - radius * 0.8),
                (cx + radius, cy),
            );
            path.curve_to(
                (cx + radius * 0.5, cy + radius * 0.8),
                (cx - radius * 0.5, cy + radius * 0.8),
                (cx - radius, cy),
            );
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                color,
                None,
                &Circle::new((cx, cy), radius * 0.26),
            );
        }
        UiIcon::Move => {
            path.move_to((cx - radius, cy));
            path.line_to((cx + radius, cy));
            path.move_to((cx, cy - radius));
            path.line_to((cx, cy + radius));
            for (x, y, dx, dy) in [
                (cx - radius, cy, 0.35, -0.35),
                (cx - radius, cy, 0.35, 0.35),
                (cx + radius, cy, -0.35, -0.35),
                (cx + radius, cy, -0.35, 0.35),
                (cx, cy - radius, -0.35, 0.35),
                (cx, cy - radius, 0.35, 0.35),
                (cx, cy + radius, -0.35, -0.35),
                (cx, cy + radius, 0.35, -0.35),
            ] {
                path.move_to((x, y));
                path.line_to((x + dx * radius, y + dy * radius));
            }
        }
        UiIcon::Undo | UiIcon::Redo => {
            let direction = if icon == UiIcon::Undo { -1.0 } else { 1.0 };
            path.move_to((cx - direction * radius * 0.75, cy - radius * 0.7));
            path.line_to((cx + direction * radius * 0.1, cy - radius * 0.7));
            path.curve_to(
                (cx + direction * radius, cy - radius * 0.7),
                (cx + direction * radius, cy + radius * 0.7),
                (cx + direction * radius * 0.1, cy + radius * 0.7),
            );
            path.move_to((cx - direction * radius * 0.75, cy - radius * 0.7));
            path.line_to((cx - direction * radius * 0.25, cy - radius));
            path.move_to((cx - direction * radius * 0.75, cy - radius * 0.7));
            path.line_to((cx - direction * radius * 0.25, cy - radius * 0.35));
        }
        UiIcon::Scale => {
            path.move_to((cx - radius, cy + radius));
            path.line_to((cx, cy - radius));
            path.line_to((cx + radius, cy + radius));
            path.move_to((cx - radius * 0.55, cy + radius * 0.2));
            path.line_to((cx + radius * 0.55, cy + radius * 0.2));
        }
        UiIcon::Snap => {
            path.move_to((cx - radius, cy - radius));
            path.line_to((cx - radius, cy + radius * 0.35));
            path.curve_to(
                (cx - radius, cy + radius),
                (cx + radius, cy + radius),
                (cx + radius, cy + radius * 0.35),
            );
            path.line_to((cx + radius, cy - radius));
            path.move_to((cx - radius, cy - radius * 0.35));
            path.line_to((cx - radius * 0.4, cy - radius * 0.35));
            path.move_to((cx + radius * 0.4, cy - radius * 0.35));
            path.line_to((cx + radius, cy - radius * 0.35));
        }
        UiIcon::Theme => {
            scene.stroke(
                &stroke,
                Affine::IDENTITY,
                color,
                None,
                &Circle::new((cx, cy), radius * 0.5),
            );
            for index in 0..8 {
                let angle = index as f64 * std::f64::consts::FRAC_PI_4;
                path.move_to((
                    cx + angle.cos() * radius * 0.68,
                    cy + angle.sin() * radius * 0.68,
                ));
                path.line_to((cx + angle.cos() * radius, cy + angle.sin() * radius));
            }
        }
        UiIcon::TextSelect => {
            path.move_to((cx - radius * 0.65, cy - radius));
            path.line_to((cx + radius * 0.65, cy - radius));
            path.move_to((cx, cy - radius));
            path.line_to((cx, cy + radius));
            path.move_to((cx - radius * 0.65, cy + radius));
            path.line_to((cx + radius * 0.65, cy + radius));
        }
        UiIcon::Wire => {
            path.move_to((cx - radius, cy + radius * 0.65));
            path.line_to((cx - radius * 0.25, cy + radius * 0.65));
            path.line_to((cx + radius * 0.25, cy - radius * 0.65));
            path.line_to((cx + radius, cy - radius * 0.65));
            for (x, y) in [
                (cx - radius, cy + radius * 0.65),
                (cx + radius, cy - radius * 0.65),
            ] {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    color,
                    None,
                    &Circle::new((x, y), 2.2),
                );
            }
        }
        UiIcon::ZoomIn | UiIcon::ZoomOut => {
            scene.stroke(
                &stroke,
                Affine::IDENTITY,
                color,
                None,
                &Circle::new((cx - radius * 0.2, cy - radius * 0.2), radius * 0.65),
            );
            path.move_to((cx + radius * 0.28, cy + radius * 0.28));
            path.line_to((cx + radius, cy + radius));
            path.move_to((cx - radius * 0.52, cy - radius * 0.2));
            path.line_to((cx + radius * 0.12, cy - radius * 0.2));
            if icon == UiIcon::ZoomIn {
                path.move_to((cx - radius * 0.2, cy - radius * 0.52));
                path.line_to((cx - radius * 0.2, cy + radius * 0.12));
            }
        }
    }
    if !path.is_empty() {
        scene.stroke(&stroke, Affine::IDENTITY, color, None, &path);
    }
}

fn change_icon(kind: ChangeKind) -> UiIcon {
    match kind {
        ChangeKind::Add => UiIcon::Add,
        ChangeKind::Delete => UiIcon::Delete,
        ChangeKind::Duplicate => UiIcon::Duplicate,
        ChangeKind::Edit => UiIcon::Edit,
        ChangeKind::External => UiIcon::External,
        ChangeKind::Move => UiIcon::Move,
        ChangeKind::Redo => UiIcon::Redo,
        ChangeKind::Transform => UiIcon::Transform,
        ChangeKind::Undo => UiIcon::Undo,
        ChangeKind::Wire => UiIcon::Wire,
    }
}

struct NativeFont {
    data: FontData,
    ui_scale: f32,
}

#[derive(Clone, Copy)]
struct TextRun {
    size: f32,
    position: (f64, f64),
    rotation_deg: f64,
    align: TextAlign,
    color: Color,
}

#[derive(Debug, Clone)]
struct SelectableText {
    text: String,
    rect: ScreenRect,
    character_x: Vec<f64>,
    select_whole: bool,
}

impl SelectableText {
    fn whole(text: impl Into<String>, rect: ScreenRect) -> Self {
        Self {
            text: text.into(),
            rect,
            character_x: vec![rect.x0, rect.x1],
            select_whole: true,
        }
    }

    fn character_count(&self) -> usize {
        self.text.chars().count()
    }

    fn character_at(&self, x: f64) -> usize {
        if self.select_whole {
            return self.character_count();
        }
        self.character_x
            .windows(2)
            .position(|window| x < (window[0] + window[1]) / 2.0)
            .unwrap_or_else(|| self.character_count())
    }

    fn selected_text(&self, start: usize, end: usize) -> String {
        let (start, end) = if self.select_whole {
            (0, self.character_count())
        } else {
            (start.min(end), start.max(end))
        };
        self.text
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect()
    }

    fn selection_rect(&self, start: usize, end: usize) -> ScreenRect {
        if self.select_whole {
            return self.rect;
        }
        let (start, end) = (start.min(end), start.max(end));
        ScreenRect {
            x0: self.character_x[start.min(self.character_x.len().saturating_sub(1))],
            y0: self.rect.y0,
            x1: self.character_x[end.min(self.character_x.len().saturating_sub(1))],
            y1: self.rect.y1,
        }
    }
}

#[derive(Debug, Clone)]
struct TextDrag {
    target: SelectableText,
    anchor: usize,
    current: usize,
}

#[derive(Debug, Clone)]
struct TextSelection {
    target: SelectableText,
    start: usize,
    end: usize,
}

impl NativeFont {
    fn load() -> Result<Self> {
        let mut database = Database::new();
        database.load_system_fonts();
        #[cfg(target_os = "windows")]
        let preferred = [Family::Name("Segoe UI"), Family::SansSerif];
        #[cfg(target_os = "macos")]
        let preferred = [Family::Name("SF Pro Text"), Family::SansSerif];
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let preferred = [Family::Name("Noto Sans"), Family::SansSerif];
        let id = database
            .query(&Query {
                families: &preferred,
                ..Query::default()
            })
            .or_else(|| database.faces().next().map(|face| face.id))
            .ok_or_else(|| anyhow!("no system font is available"))?;
        let (bytes, index) = database
            .with_face_data(id, |data, index| (data.to_vec(), index))
            .ok_or_else(|| anyhow!("failed to load the selected system font"))?;
        Ok(Self {
            data: FontData::new(Blob::new(Arc::new(bytes)), index),
            ui_scale: 1.0,
        })
    }

    fn measure_and_glyphs(&self, size: f32, text: &str) -> (Vec<Glyph>, f32) {
        let Some(face) = ShapingFace::from_slice(self.data.data.as_ref(), self.data.index) else {
            return (Vec::new(), 0.0);
        };
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.guess_segment_properties();
        let shaped = shape(&face, &[], buffer);
        let scale = size / face.units_per_em() as f32;
        let mut advance = 0.0_f32;
        let glyphs = shaped
            .glyph_infos()
            .iter()
            .zip(shaped.glyph_positions())
            .map(|(info, position)| {
                let glyph = Glyph {
                    id: info.glyph_id,
                    x: advance + position.x_offset as f32 * scale,
                    y: -(position.y_offset as f32) * scale,
                };
                advance += position.x_advance as f32 * scale;
                glyph
            })
            .collect();
        (glyphs, advance)
    }

    fn draw_with_target(
        &self,
        scene: &mut Scene,
        text: &str,
        run: TextRun,
    ) -> Option<SelectableText> {
        let size = run.size * self.ui_scale;
        if text.is_empty() || size <= 0.0 {
            return None;
        }
        let (glyphs, width) = self.measure_and_glyphs(size, text);
        if glyphs.is_empty() {
            return None;
        }
        let offset = match run.align {
            TextAlign::Left => 0.0,
            TextAlign::Center => f64::from(width) / 2.0,
            TextAlign::Right => f64::from(width),
        };
        let transform =
            Affine::translate((run.position.0, run.position.1 + f64::from(size) * 0.34))
                * Affine::rotate(-run.rotation_deg.to_radians())
                * Affine::translate((-offset, 0.0));
        let target = (run.rotation_deg.abs() <= f64::EPSILON).then(|| {
            let x0 = run.position.0 - offset;
            let count = text.chars().count();
            let character_x = (0..=count)
                .map(|index| x0 + f64::from(width) * index as f64 / count.max(1) as f64)
                .collect();
            SelectableText {
                text: text.to_owned(),
                rect: ScreenRect {
                    x0,
                    y0: run.position.1 - f64::from(size) * 0.58,
                    x1: x0 + f64::from(width),
                    y1: run.position.1 + f64::from(size) * 0.58,
                },
                character_x,
                select_whole: false,
            }
        });
        scene
            .draw_glyphs(&self.data)
            .font_size(size)
            .transform(transform)
            .brush(run.color)
            .draw(Fill::NonZero, glyphs.into_iter());
        target
    }
}

fn draw_selectable_text(
    font: &NativeFont,
    scene: &mut Scene,
    targets: &mut Vec<SelectableText>,
    text: &str,
    run: TextRun,
) {
    if let Some(target) = font.draw_with_target(scene, text, run) {
        targets.push(target);
    }
}

struct NativeSheet {
    name: String,
    depth: usize,
    file: PathBuf,
    semantic: SchematicScene,
    rendered: Scene,
    compatibility: Option<Scene>,
    compatibility_error: Option<String>,
}

impl NativeSheet {
    fn from_hierarchy(entry: HierarchyScene, palette: Palette) -> Self {
        let rendered = encode_scene(&entry.scene, palette);
        let compatibility = (!entry.scene.coverage.is_complete())
            .then(|| crate::svg_order_cache::load_fresh(&entry.file))
            .flatten()
            .and_then(|svg| compatibility_scene(&svg).ok());
        Self {
            name: entry.name,
            depth: entry.depth,
            file: entry.file,
            semantic: entry.scene,
            rendered,
            compatibility,
            compatibility_error: None,
        }
    }

    fn rebuild(&mut self, palette: Palette) {
        self.rendered = encode_scene(&self.semantic, palette);
    }
}

struct RenderState {
    surface: RenderSurface<'static>,
    window: Arc<Window>,
    valid_surface: bool,
}

enum UserEvent {
    FilesChanged(Vec<PathBuf>),
    Reloaded(ReloadBatch),
}

#[derive(Debug)]
struct ReloadRequest {
    generation: u64,
    root: PathBuf,
    changed: HashSet<PathBuf>,
    known: HashSet<PathBuf>,
    external: bool,
}

struct ReloadBatch {
    generation: u64,
    entries: std::result::Result<Vec<HierarchyEntry>, String>,
    loaded: HashMap<PathBuf, std::result::Result<LoadedScene, String>>,
    external: bool,
}

struct LoadedScene {
    semantic: SchematicScene,
    compatibility: Option<Scene>,
    compatibility_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolbarAction {
    Undo,
    Redo,
    ZoomIn,
    ZoomOut,
    Fit,
    Grid,
    Snap,
    UiScale,
    HighlightChanges,
    FollowChanges,
    TextSelect,
    Theme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditControl {
    EditMode,
    Commit,
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiIcon {
    Add,
    Commit,
    Delete,
    Discard,
    Duplicate,
    Edit,
    External,
    Fit,
    Follow,
    Grid,
    Highlight,
    Move,
    Redo,
    Scale,
    Snap,
    Theme,
    TextSelect,
    Transform,
    Undo,
    Wire,
    ZoomIn,
    ZoomOut,
}

#[derive(Debug, Clone, Copy)]
struct SelectionBox {
    start: PhysicalPosition<f64>,
    current: PhysicalPosition<f64>,
    additive: bool,
}

struct ItemDrag {
    start: PhysicalPosition<f64>,
    current: PhysicalPosition<f64>,
    base_scene: Scene,
    connected_wires: Vec<ConnectedWireMove>,
}

#[derive(Debug, Clone, Copy)]
struct DiagnosticsPanelDrag {
    pointer_offset: (f64, f64),
}

#[derive(Debug, Clone)]
struct SearchHit {
    sheet: usize,
    uuid: Option<String>,
    description: String,
}

#[derive(Debug, Clone, Default)]
struct SearchState {
    query: String,
    hits: Vec<SearchHit>,
    current: usize,
}

#[derive(Debug, Clone)]
struct PropertyEdit {
    file: PathBuf,
    uuid: String,
    name: String,
    value: String,
}

#[derive(Debug, Clone, Copy)]
struct WireDraft {
    start: SchPoint,
    current: SchPoint,
    is_bus: bool,
}

#[derive(Debug, Clone)]
struct LabelEdit {
    point: SchPoint,
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SheetEditField {
    Name,
    File,
}

#[derive(Debug, Clone)]
struct SheetEdit {
    point: SchPoint,
    name: String,
    file: String,
    field: SheetEditField,
}

#[derive(Debug, Clone)]
struct SheetPinEdit {
    sheet_uuid: String,
    point: SchPoint,
    rotation: f64,
    name: String,
    pin_type: SheetPinType,
}

#[derive(Debug, Clone)]
struct ExternalChangePreview {
    lines: Vec<String>,
}

struct VelloViewer {
    root: PathBuf,
    font: NativeFont,
    theme: Theme,
    sheets: Vec<NativeSheet>,
    active: usize,
    selected_uuids: HashSet<String>,
    status: String,
    settings: ViewerSettings,
    timeline: ChangeTimeline,
    highlighted_change: Option<u64>,
    pending_follow: Option<u64>,
    undo_stack: Vec<HistoryEntry>,
    redo_stack: Vec<HistoryEntry>,
    local_revisions: HashMap<PathBuf, DocumentRevision>,
    edit_session: EditSession,

    context: RenderContext,
    renderers: Vec<Option<Renderer>>,
    state: Option<RenderState>,
    cached_window: Option<Arc<Window>>,
    frame: Scene,

    watcher: RecommendedWatcher,
    watched_dirs: HashSet<PathBuf>,
    reload_tx: mpsc::Sender<ReloadRequest>,
    reload_generation: Arc<AtomicU64>,

    cursor: Option<PhysicalPosition<f64>>,
    modifiers: ModifiersState,
    panning: bool,
    selection_box: Option<SelectionBox>,
    item_drag: Option<ItemDrag>,
    diagnostics_drag: Option<DiagnosticsPanelDrag>,
    search: Option<SearchState>,
    property_edit: Option<PropertyEdit>,
    wire_draft: Option<WireDraft>,
    label_edit: Option<LabelEdit>,
    sheet_edit: Option<SheetEdit>,
    sheet_pin_edit: Option<SheetPinEdit>,
    external_preview: Option<ExternalChangePreview>,
    text_targets: Vec<SelectableText>,
    text_select_mode: bool,
    text_drag: Option<TextDrag>,
    text_selection: Option<TextSelection>,
    clipboard: Option<arboard::Clipboard>,
    pan: (f64, f64),
    zoom: f64,
    grid_mm: f64,
    bus_entry_direction: BusEntryDirection,
    snap_enabled: bool,
    film_scroll: f64,
    timeline_scroll: usize,
}

impl VelloViewer {
    fn new(
        root: PathBuf,
        mut font: NativeFont,
        sheets: Vec<NativeSheet>,
        watcher: RecommendedWatcher,
        reload_tx: mpsc::Sender<ReloadRequest>,
        reload_generation: Arc<AtomicU64>,
    ) -> Self {
        let settings = ViewerSettings::load();
        font.ui_scale = settings.ui_scale;
        let theme = if settings.dark_theme {
            Theme::Dark
        } else {
            Theme::Light
        };
        let mut viewer = Self {
            root,
            font,
            theme,
            sheets,
            active: 0,
            selected_uuids: HashSet::new(),
            status: "Live".to_owned(),
            settings,
            timeline: ChangeTimeline::default(),
            highlighted_change: None,
            pending_follow: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            local_revisions: HashMap::new(),
            edit_session: EditSession::default(),
            context: RenderContext::new(),
            renderers: Vec::new(),
            state: None,
            cached_window: None,
            frame: Scene::new(),
            watcher,
            watched_dirs: HashSet::new(),
            reload_tx,
            reload_generation,
            cursor: None,
            modifiers: ModifiersState::empty(),
            panning: false,
            selection_box: None,
            item_drag: None,
            diagnostics_drag: None,
            search: None,
            property_edit: None,
            wire_draft: None,
            label_edit: None,
            sheet_edit: None,
            sheet_pin_edit: None,
            external_preview: None,
            text_targets: Vec::new(),
            text_select_mode: false,
            text_drag: None,
            text_selection: None,
            clipboard: arboard::Clipboard::new().ok(),
            pan: (0.0, 0.0),
            zoom: 1.0,
            grid_mm: 1.27,
            bus_entry_direction: BusEntryDirection::DownRight,
            snap_enabled: true,
            film_scroll: 0.0,
            timeline_scroll: 0,
        };
        viewer.reconcile_watch_dirs();
        viewer.status = viewer.live_status();
        viewer
    }

    fn live_status(&self) -> String {
        let mode = if self.edit_session.enabled {
            let pending = self.edit_session.dirty_document_count();
            if pending == 0 {
                "Edit mode".to_owned()
            } else {
                format!("Edit mode · {pending} staged file(s)")
            }
        } else {
            "Read-only".to_owned()
        };
        let mut base = format!("{mode} · Live · {} page(s)", self.sheets.len());
        let Some(sheet) = self.sheets.get(self.active) else {
            return base;
        };
        if !sheet.semantic.diagnostics.is_empty() {
            base.push_str(&format!(
                " · {} design warning(s)",
                sheet.semantic.diagnostics.len()
            ));
        }
        if sheet.semantic.coverage.is_complete() {
            return base;
        }
        if sheet.compatibility.is_some() {
            return format!("{base} · KiCad SVG fallback active for this sheet");
        }
        let kinds = sheet
            .semantic
            .coverage
            .unsupported
            .iter()
            .map(|construct| format!("{}×{}", construct.kind, construct.count))
            .collect::<Vec<_>>()
            .join(", ");
        match &sheet.compatibility_error {
            Some(error) => format!("{base} · native fallback ({kinds}) · fallback error: {error}"),
            None => format!("{base} · preparing KiCad SVG fallback: {kinds}"),
        }
    }

    fn request_redraw(&self) {
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }

    fn remember_local_revision(&mut self, file: &Path, revision: DocumentRevision) {
        self.local_revisions.insert(path_key(file), revision);
    }

    fn apply_staged_source(&mut self, file: &Path, source: String) -> bool {
        let Ok(semantic) = SchematicScene::from_source(file, source) else {
            return false;
        };
        let palette = self.palette();
        let rendered = encode_scene(&semantic, palette);
        let Some(sheet) = self
            .sheets
            .iter_mut()
            .find(|sheet| path_key(&sheet.file) == path_key(file))
        else {
            return false;
        };
        sheet.semantic = semantic;
        sheet.rendered = rendered;
        // A KiCad SVG fallback reflects the durable file, not the staged
        // source. Keep the semantic preview authoritative until Commit.
        sheet.compatibility = None;
        sheet.compatibility_error = None;
        true
    }

    fn stage_command(
        &mut self,
        file: &Path,
        command: &SchematicCommand,
    ) -> std::result::Result<konnect_sexp::TransactionOutcome, SexpError> {
        let source = self
            .sheets
            .iter()
            .find(|sheet| path_key(&sheet.file) == path_key(file))
            .map(|sheet| sheet.semantic.source.to_string())
            .ok_or_else(|| SexpError::InvalidValue("staged sheet is not loaded".to_owned()))?;
        let key = path_key(file);
        let mut candidate = self.edit_session.clone();
        let (replacement, outcome) = candidate.stage_command(key, file, &source, command)?;
        if !self.apply_staged_source(file, replacement) {
            return Err(SexpError::InvalidValue(
                "staged schematic could not be rendered".to_owned(),
            ));
        }
        self.edit_session = candidate;
        Ok(outcome)
    }

    fn require_edit_mode(&mut self) -> bool {
        if self.edit_session.enabled {
            true
        } else {
            self.status = "Read-only · enable Edit mode before changing the schematic".to_owned();
            false
        }
    }

    fn toggle_edit_mode(&mut self) {
        if self.edit_session.enabled && self.edit_session.has_pending() {
            self.status = "Commit or Discard staged changes before leaving Edit mode".to_owned();
            return;
        }
        self.edit_session.enabled = !self.edit_session.enabled;
        self.status = if self.edit_session.enabled {
            "Edit mode · changes stay in memory until Commit".to_owned()
        } else {
            "Read-only mode · schematic editing is disabled".to_owned()
        };
    }

    fn commit_edit_session(&mut self) {
        if !self.edit_session.enabled {
            self.status = "Enable Edit mode before committing".to_owned();
            return;
        }
        if !self.edit_session.has_pending() {
            self.status = "Nothing staged to commit".to_owned();
            return;
        }
        if self.edit_session.is_conflicted() {
            self.status = format!(
                "Commit blocked · {} staged file(s) changed externally; Discard and review the new source",
                self.edit_session.conflicted_count()
            );
            return;
        }
        if self
            .edit_session
            .dirty_documents()
            .any(|document| kicad_lock_path(&document.file).exists())
        {
            self.status = "Commit blocked · close staged sheets in KiCad first".to_owned();
            return;
        }
        let transitions = self.edit_session.transitions();
        let files = self
            .edit_session
            .dirty_documents()
            .map(|document| document.file.clone())
            .collect::<Vec<_>>();
        let Some(journal_root) = self.root.parent().map(Path::to_path_buf) else {
            self.status = "Commit blocked · project has no transaction directory".to_owned();
            return;
        };
        match commit_file_transaction(&journal_root, transitions) {
            Ok(_) => {
                let revisions = self
                    .edit_session
                    .dirty_documents()
                    .map(|document| {
                        (
                            document.file.clone(),
                            DocumentRevision::of(&document.staged),
                        )
                    })
                    .collect::<Vec<_>>();
                for (file, revision) in &revisions {
                    self.remember_local_revision(file, *revision);
                }
                let edit_count = self.undo_stack.len();
                self.edit_session.clear();
                self.undo_stack.clear();
                self.redo_stack.clear();
                self.record_change(
                    ChangeOrigin::Local,
                    format!("Committed {edit_count} staged edit(s)"),
                    self.root.clone(),
                    Vec::new(),
                );
                self.schedule_reload(&files);
                self.status = format!(
                    "Committed {edit_count} staged edit(s) across {} file(s) atomically",
                    files.len()
                );
            }
            Err(error) => {
                self.status =
                    format!("Commit stopped safely: {error} · staged changes remain available");
            }
        }
    }

    fn discard_edit_session(&mut self) {
        if !self.edit_session.has_pending() {
            self.status = "Nothing staged to discard".to_owned();
            return;
        }
        let files = self
            .edit_session
            .dirty_documents()
            .map(|document| document.file.clone())
            .collect::<Vec<_>>();
        let load_as_external = self.edit_session.is_conflicted();
        self.edit_session.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.selected_uuids.clear();
        if load_as_external {
            self.schedule_external_reload(&files);
        } else {
            self.schedule_reload(&files);
        }
        self.status = "Discarded all staged changes · durable files were untouched".to_owned();
    }

    fn record_change(
        &mut self,
        origin: ChangeOrigin,
        label: impl Into<String>,
        file: PathBuf,
        uuids: Vec<String>,
    ) -> u64 {
        let id = self.timeline.push(origin, label, file, uuids);
        self.timeline_scroll = 0;
        if self.settings.highlight_changes {
            self.highlighted_change = Some(id);
        }
        id
    }

    fn record_command_change(
        &mut self,
        origin: ChangeOrigin,
        file: &Path,
        command: &SchematicCommand,
    ) -> u64 {
        self.record_change(
            origin,
            command.label.clone(),
            file.to_path_buf(),
            command
                .changes
                .iter()
                .map(|change| change.id.as_str().to_owned())
                .collect(),
        )
    }

    fn navigate_to_change(&mut self, id: u64) {
        if self.timeline.event(id).is_none() {
            return;
        }
        self.highlighted_change = self.settings.highlight_changes.then_some(id);
        self.pending_follow = Some(id);
        self.status = "Navigating to timeline change".to_owned();
    }

    fn apply_pending_follow(&mut self, width: f64, height: f64) {
        let Some(id) = self.pending_follow.take() else {
            return;
        };
        let Some(event) = self.timeline.event(id) else {
            return;
        };
        let file = path_key(&event.file);
        let uuids = event.uuids.clone();
        let Some(index) = self
            .sheets
            .iter()
            .position(|sheet| path_key(&sheet.file) == file)
        else {
            return;
        };
        self.active = index;
        self.selected_uuids.clear();
        let bounds = self.sheets[index]
            .semantic
            .objects
            .iter()
            .filter(|object| uuids.contains(&object.uuid))
            .map(|object| object.bounds)
            .reduce(union_bounds);
        let Some(bounds) = bounds else {
            self.fit();
            return;
        };
        self.zoom = 3.0;
        let sheet = &self.sheets[index];
        let area = self.main_rect(width, height);
        let fit = ((area.width() - PAGE_PADDING * 2.0) / sheet.semantic.width_mm)
            .min((area.height() - PAGE_PADDING * 2.0) / sheet.semantic.height_mm)
            .max(0.001);
        let scale = fit * self.zoom;
        let target_x = (bounds.min_x + bounds.max_x) / 2.0;
        let target_y = (bounds.min_y + bounds.max_y) / 2.0;
        self.pan = (
            (sheet.semantic.width_mm / 2.0 - target_x) * scale,
            (sheet.semantic.height_mm / 2.0 - target_y) * scale,
        );
    }

    fn focus_point(&mut self, width: f64, height: f64, point: SchPoint) {
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        self.zoom = 4.0;
        let area = self.main_rect(width, height);
        let fit = ((area.width() - PAGE_PADDING * 2.0) / sheet.semantic.width_mm)
            .min((area.height() - PAGE_PADDING * 2.0) / sheet.semantic.height_mm)
            .max(0.001);
        let scale = fit * self.zoom;
        self.pan = (
            (sheet.semantic.width_mm / 2.0 - point.x) * scale,
            (sheet.semantic.height_mm / 2.0 - point.y) * scale,
        );
    }

    fn diagnostics_panel_rect(
        &self,
        width: f64,
        height: f64,
        diagnostic_count: usize,
    ) -> ScreenRect {
        let x_min = SIDEBAR_WIDTH + 14.0;
        let y_min = STATUS_HEIGHT + 8.0;
        let available_right = (width - 14.0).max(x_min + 1.0);
        let requested_width = if self.settings.diagnostics_collapsed {
            244.0
        } else {
            DIAGNOSTICS_PANEL_WIDTH
        };
        let panel_width = requested_width.min(available_right - x_min);
        let visible = diagnostic_count.min(4);
        let line_height = 36.0 * f64::from(self.settings.ui_scale).min(1.35);
        let requested_height = if self.settings.diagnostics_collapsed {
            DIAGNOSTICS_HEADER_HEIGHT
        } else {
            46.0 + visible as f64 * line_height
        };
        let available_bottom =
            (height - FILMSTRIP_HEIGHT - 8.0).max(y_min + DIAGNOSTICS_HEADER_HEIGHT);
        let panel_height = requested_height.min(available_bottom - y_min);
        let x_max = (available_right - panel_width).max(x_min);
        let y_max = (available_bottom - panel_height).max(y_min);
        let (x_fraction, y_fraction) = if self.settings.diagnostics_collapsed {
            (
                self.settings.diagnostics_collapsed_x.unwrap_or(1.0),
                self.settings.diagnostics_collapsed_y.unwrap_or(0.0),
            )
        } else {
            (
                self.settings.diagnostics_panel_x.unwrap_or(0.0),
                self.settings.diagnostics_panel_y.unwrap_or(0.0),
            )
        };
        let x0 = x_min + (x_max - x_min) * f64::from(x_fraction);
        let y0 = y_min + (y_max - y_min) * f64::from(y_fraction);
        ScreenRect {
            x0,
            y0,
            x1: x0 + panel_width,
            y1: y0 + panel_height,
        }
    }

    fn diagnostics_collapse_rect(panel: ScreenRect) -> ScreenRect {
        ScreenRect {
            x0: panel.x1 - 32.0,
            y0: panel.y0 + 7.0,
            x1: panel.x1 - 8.0,
            y1: panel.y0 + 31.0,
        }
    }

    fn move_diagnostics_panel(
        &mut self,
        width: f64,
        height: f64,
        position: PhysicalPosition<f64>,
        drag: DiagnosticsPanelDrag,
    ) {
        let diagnostic_count = self
            .sheets
            .get(self.active)
            .map_or(0, |sheet| sheet.semantic.diagnostics.len());
        let panel = self.diagnostics_panel_rect(width, height, diagnostic_count);
        let x_min = SIDEBAR_WIDTH + 14.0;
        let y_min = STATUS_HEIGHT + 8.0;
        let x_max = (width - 14.0 - panel.width()).max(x_min);
        let available_bottom =
            (height - FILMSTRIP_HEIGHT - 8.0).max(y_min + DIAGNOSTICS_HEADER_HEIGHT);
        let y_max = (available_bottom - panel.height()).max(y_min);
        let x0 = (position.x - drag.pointer_offset.0).clamp(x_min, x_max);
        let y0 = (position.y - drag.pointer_offset.1).clamp(y_min, y_max);
        let x_fraction = if x_max > x_min {
            ((x0 - x_min) / (x_max - x_min)) as f32
        } else {
            0.0
        };
        let y_fraction = if y_max > y_min {
            ((y0 - y_min) / (y_max - y_min)) as f32
        } else {
            0.0
        };
        if self.settings.diagnostics_collapsed {
            self.settings.diagnostics_collapsed_x = Some(x_fraction);
            self.settings.diagnostics_collapsed_y = Some(y_fraction);
        } else {
            self.settings.diagnostics_panel_x = Some(x_fraction);
            self.settings.diagnostics_panel_y = Some(y_fraction);
        }
        self.status = "Moving design checks · release to keep position".to_owned();
    }

    fn finish_diagnostics_drag(&mut self) {
        if self.diagnostics_drag.take().is_some() && self.persist_settings() {
            self.status = "Design checks · panel position saved".to_owned();
        }
    }

    fn handle_diagnostics(&mut self, width: f64, height: f64, x: f64, y: f64) -> bool {
        let diagnostic_count = self
            .sheets
            .get(self.active)
            .map_or(0, |sheet| sheet.semantic.diagnostics.len());
        if diagnostic_count == 0 {
            return false;
        }
        let panel = self.diagnostics_panel_rect(width, height, diagnostic_count);
        if !panel.contains(x, y) {
            return false;
        }
        if Self::diagnostics_collapse_rect(panel).contains(x, y) {
            self.settings.diagnostics_collapsed = !self.settings.diagnostics_collapsed;
            if self.persist_settings() {
                self.status = if self.settings.diagnostics_collapsed {
                    "Design checks · collapsed".to_owned()
                } else {
                    "Design checks · expanded".to_owned()
                };
            }
            return true;
        }
        if y <= panel.y0 + DIAGNOSTICS_HEADER_HEIGHT {
            self.diagnostics_drag = Some(DiagnosticsPanelDrag {
                pointer_offset: (x - panel.x0, y - panel.y0),
            });
            self.status = "Moving design checks · drag anywhere by the header".to_owned();
            return true;
        }
        if self.settings.diagnostics_collapsed {
            return true;
        }
        let line_height = 36.0 * f64::from(self.settings.ui_scale).min(1.35);
        let row = ((y - (panel.y0 + DIAGNOSTICS_HEADER_HEIGHT)) / line_height).floor() as usize;
        if let Some(diagnostic) = self
            .sheets
            .get(self.active)
            .and_then(|sheet| sheet.semantic.diagnostics.get(row))
            .cloned()
        {
            self.focus_point(width, height, diagnostic.point);
            self.status = format!("Design check · {}", diagnostic.message);
        }
        true
    }

    fn external_paths(&mut self, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths
            .into_iter()
            .filter(|path| {
                let key = path_key(path);
                let Some(expected) = self.local_revisions.get(&key).copied() else {
                    return true;
                };
                let matches_local = notification_matches_revision(path, expected);
                if !matches_local {
                    self.local_revisions.remove(&key);
                }
                !matches_local
            })
            .collect()
    }

    fn palette(&self) -> Palette {
        self.theme.palette()
    }

    fn main_rect(&self, width: f64, height: f64) -> ScreenRect {
        ScreenRect {
            x0: SIDEBAR_WIDTH,
            y0: STATUS_HEIGHT,
            x1: width,
            y1: (height - FILMSTRIP_HEIGHT).max(STATUS_HEIGHT + 1.0),
        }
    }

    fn filmstrip_rect(&self, width: f64, height: f64) -> ScreenRect {
        ScreenRect {
            x0: SIDEBAR_WIDTH,
            y0: (height - FILMSTRIP_HEIGHT).max(STATUS_HEIGHT),
            x1: width,
            y1: height,
        }
    }

    fn page_transform(&self, width: f64, height: f64) -> Option<(Affine, f64, f64, f64)> {
        let sheet = self.sheets.get(self.active)?;
        let area = self.main_rect(width, height);
        let fit = ((area.width() - PAGE_PADDING * 2.0) / sheet.semantic.width_mm)
            .min((area.height() - PAGE_PADDING * 2.0) / sheet.semantic.height_mm)
            .max(0.001);
        let scale = fit * self.zoom;
        let (center_x, center_y) = area.center();
        let x = center_x + self.pan.0 - sheet.semantic.width_mm * scale / 2.0;
        let y = center_y + self.pan.1 - sheet.semantic.height_mm * scale / 2.0;
        Some((Affine::new([scale, 0.0, 0.0, scale, x, y]), scale, x, y))
    }

    fn schematic_point(&self, width: f64, height: f64, x: f64, y: f64) -> Option<SchPoint> {
        let (_, scale, tx, ty) = self.page_transform(width, height)?;
        Some(SchPoint {
            x: (x - tx) / scale,
            y: (y - ty) / scale,
        })
    }

    fn set_zoom_about(&mut self, width: f64, height: f64, new_zoom: f64, x: f64, y: f64) {
        let Some(world) = self.schematic_point(width, height, x, y) else {
            return;
        };
        self.zoom = new_zoom.clamp(0.1, 40.0);
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let area = self.main_rect(width, height);
        let fit = ((area.width() - PAGE_PADDING * 2.0) / sheet.semantic.width_mm)
            .min((area.height() - PAGE_PADDING * 2.0) / sheet.semantic.height_mm)
            .max(0.001);
        let scale = fit * self.zoom;
        let (center_x, center_y) = area.center();
        self.pan.0 = x - center_x - (world.x - sheet.semantic.width_mm / 2.0) * scale;
        self.pan.1 = y - center_y - (world.y - sheet.semantic.height_mm / 2.0) * scale;
    }

    fn fit(&mut self) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
    }

    fn switch_sheet(&mut self, index: usize) {
        if index >= self.sheets.len() || index == self.active {
            return;
        }
        self.active = index;
        self.selected_uuids.clear();
        self.fit();
        self.status = self.live_status();
    }

    fn toggle_theme(&mut self) {
        self.theme = match self.theme {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        };
        self.settings.dark_theme = self.theme == Theme::Dark;
        let palette = self.palette();
        for sheet in &mut self.sheets {
            sheet.rebuild(palette);
        }
        if self.persist_settings() {
            self.status = format!(
                "Theme · {}",
                if self.settings.dark_theme {
                    "high-contrast dark"
                } else {
                    "KiCad light"
                }
            );
        }
    }

    fn edit_controls(width: f64) -> [(EditControl, ScreenRect); 3] {
        const GAP: f64 = 6.0;
        const EDIT_WIDTH: f64 = 112.0;
        const ACTION_WIDTH: f64 = 82.0;
        let right = width - 10.0;
        let discard_x = right - ACTION_WIDTH;
        let commit_x = discard_x - GAP - ACTION_WIDTH;
        let edit_x = commit_x - GAP - EDIT_WIDTH;
        [
            (
                EditControl::EditMode,
                ScreenRect {
                    x0: edit_x,
                    y0: 6.0,
                    x1: edit_x + EDIT_WIDTH,
                    y1: 34.0,
                },
            ),
            (
                EditControl::Commit,
                ScreenRect {
                    x0: commit_x,
                    y0: 6.0,
                    x1: commit_x + ACTION_WIDTH,
                    y1: 34.0,
                },
            ),
            (
                EditControl::Discard,
                ScreenRect {
                    x0: discard_x,
                    y0: 6.0,
                    x1: discard_x + ACTION_WIDTH,
                    y1: 34.0,
                },
            ),
        ]
    }

    fn handle_edit_controls(&mut self, width: f64, x: f64, y: f64) -> bool {
        let Some((control, _)) = Self::edit_controls(width)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
        else {
            return false;
        };
        match control {
            EditControl::EditMode => self.toggle_edit_mode(),
            EditControl::Commit => self.commit_edit_session(),
            EditControl::Discard => self.discard_edit_session(),
        }
        true
    }

    fn draw_edit_controls(&mut self, width: f64, palette: Palette) {
        let hovered = self.cursor.and_then(|cursor| {
            Self::edit_controls(width)
                .into_iter()
                .find(|(_, rect)| rect.contains(cursor.x, cursor.y))
                .map(|(control, _)| control)
        });
        let pending = self.edit_session.dirty_document_count();
        for (control, rect) in Self::edit_controls(width) {
            let active = control == EditControl::EditMode && self.edit_session.enabled;
            let actionable = control == EditControl::EditMode || pending > 0;
            let border = if self.edit_session.is_conflicted()
                && matches!(control, EditControl::Commit | EditControl::Discard)
            {
                palette.selection
            } else if active || hovered == Some(control) {
                palette.accent
            } else {
                palette.card_border
            };
            self.frame.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                if active {
                    palette.accent.with_alpha(0.16)
                } else {
                    palette.card.with_alpha(if actionable { 0.96 } else { 0.5 })
                },
                None,
                &RoundedRect::new(rect.x0, rect.y0, rect.x1, rect.y1, 6.0),
            );
            self.frame.stroke(
                &Stroke::new(if active || hovered == Some(control) {
                    1.5
                } else {
                    1.0
                }),
                Affine::IDENTITY,
                border,
                None,
                &RoundedRect::new(rect.x0, rect.y0, rect.x1, rect.y1, 6.0),
            );
            let text_color = if actionable {
                palette.text
            } else {
                palette.text.with_alpha(0.48)
            };
            let (label, text_x) = match control {
                EditControl::EditMode => {
                    let checkbox = ScreenRect {
                        x0: rect.x0 + 8.0,
                        y0: rect.y0 + 7.0,
                        x1: rect.x0 + 22.0,
                        y1: rect.y0 + 21.0,
                    };
                    self.frame.stroke(
                        &Stroke::new(1.4),
                        Affine::IDENTITY,
                        if active {
                            palette.accent
                        } else {
                            palette.card_border
                        },
                        None,
                        &RoundedRect::new(checkbox.x0, checkbox.y0, checkbox.x1, checkbox.y1, 2.5),
                    );
                    if active {
                        draw_ui_icon(
                            &mut self.frame,
                            UiIcon::Commit,
                            ScreenRect {
                                x0: checkbox.x0 + 2.0,
                                y0: checkbox.y0 + 2.0,
                                x1: checkbox.x1 - 2.0,
                                y1: checkbox.y1 - 2.0,
                            },
                            palette.accent,
                        );
                    }
                    ("Edit mode".to_owned(), rect.x0 + 29.0)
                }
                EditControl::Commit => {
                    draw_ui_icon(
                        &mut self.frame,
                        UiIcon::Commit,
                        ScreenRect {
                            x0: rect.x0 + 7.0,
                            y0: rect.y0 + 7.0,
                            x1: rect.x0 + 21.0,
                            y1: rect.y0 + 21.0,
                        },
                        text_color,
                    );
                    (
                        if pending == 0 {
                            "Commit".to_owned()
                        } else {
                            format!("Commit {pending}")
                        },
                        rect.x0 + 27.0,
                    )
                }
                EditControl::Discard => {
                    draw_ui_icon(
                        &mut self.frame,
                        UiIcon::Discard,
                        ScreenRect {
                            x0: rect.x0 + 7.0,
                            y0: rect.y0 + 7.0,
                            x1: rect.x0 + 21.0,
                            y1: rect.y0 + 21.0,
                        },
                        text_color,
                    );
                    ("Discard".to_owned(), rect.x0 + 27.0)
                }
            };
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &label,
                TextRun {
                    size: 10.5,
                    position: (text_x, rect.center().1),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: text_color,
                },
            );
        }
    }

    fn toolbar_buttons(_width: f64) -> [(ToolbarAction, ScreenRect); 12] {
        std::array::from_fn(|index| {
            const BUTTON_SIZE: f64 = 36.0;
            const COLUMN_GAP: f64 = 8.0;
            const ROW_GAP: f64 = 7.0;
            let column = index % 2;
            let row = index / 2;
            let x0 = 8.0 + column as f64 * (BUTTON_SIZE + COLUMN_GAP);
            let y0 = 52.0 + row as f64 * (BUTTON_SIZE + ROW_GAP);
            let rect = ScreenRect {
                x0,
                y0,
                x1: x0 + BUTTON_SIZE,
                y1: y0 + BUTTON_SIZE,
            };
            (
                [
                    ToolbarAction::Undo,
                    ToolbarAction::Redo,
                    ToolbarAction::ZoomIn,
                    ToolbarAction::ZoomOut,
                    ToolbarAction::Fit,
                    ToolbarAction::Grid,
                    ToolbarAction::Snap,
                    ToolbarAction::UiScale,
                    ToolbarAction::HighlightChanges,
                    ToolbarAction::FollowChanges,
                    ToolbarAction::TextSelect,
                    ToolbarAction::Theme,
                ][index],
                rect,
            )
        })
    }

    fn toolbar_icon(action: ToolbarAction) -> UiIcon {
        match action {
            ToolbarAction::Undo => UiIcon::Undo,
            ToolbarAction::Redo => UiIcon::Redo,
            ToolbarAction::ZoomIn => UiIcon::ZoomIn,
            ToolbarAction::ZoomOut => UiIcon::ZoomOut,
            ToolbarAction::Fit => UiIcon::Fit,
            ToolbarAction::Grid => UiIcon::Grid,
            ToolbarAction::Snap => UiIcon::Snap,
            ToolbarAction::UiScale => UiIcon::Scale,
            ToolbarAction::HighlightChanges => UiIcon::Highlight,
            ToolbarAction::FollowChanges => UiIcon::Follow,
            ToolbarAction::TextSelect => UiIcon::TextSelect,
            ToolbarAction::Theme => UiIcon::Theme,
        }
    }

    fn toolbar_label(&self, action: ToolbarAction) -> String {
        match action {
            ToolbarAction::Undo => "Undo · Ctrl/Command Z".to_owned(),
            ToolbarAction::Redo => "Redo · Ctrl/Command Shift Z".to_owned(),
            ToolbarAction::ZoomIn => "Zoom in".to_owned(),
            ToolbarAction::ZoomOut => "Zoom out".to_owned(),
            ToolbarAction::Fit => "Fit active sheet · 0".to_owned(),
            ToolbarAction::Grid => format!("Grid · {:.3} mm · G", self.grid_mm),
            ToolbarAction::Snap => format!(
                "Snap · {} · S",
                if self.snap_enabled { "on" } else { "off" }
            ),
            ToolbarAction::UiScale => {
                format!("Interface scale · {:.0}%", self.settings.ui_scale * 100.0)
            }
            ToolbarAction::HighlightChanges => format!(
                "Highlight changes · {}",
                if self.settings.highlight_changes {
                    "on"
                } else {
                    "off"
                }
            ),
            ToolbarAction::FollowChanges => format!(
                "Follow external changes · {}",
                if self.settings.follow_changes {
                    "on"
                } else {
                    "off"
                }
            ),
            ToolbarAction::TextSelect => format!(
                "Select text · {} · Ctrl/Command C to copy",
                if self.text_select_mode { "on" } else { "off" }
            ),
            ToolbarAction::Theme => format!(
                "Theme · {}",
                if self.settings.dark_theme {
                    "high-contrast dark"
                } else {
                    "KiCad light"
                }
            ),
        }
    }

    fn toolbar_action_is_active(&self, action: ToolbarAction) -> bool {
        match action {
            ToolbarAction::Snap => self.snap_enabled,
            ToolbarAction::HighlightChanges => self.settings.highlight_changes,
            ToolbarAction::FollowChanges => self.settings.follow_changes,
            ToolbarAction::TextSelect => self.text_select_mode,
            ToolbarAction::Theme => self.settings.dark_theme,
            _ => false,
        }
    }

    fn handle_toolbar(&mut self, width: f64, height: f64, x: f64, y: f64) -> bool {
        let Some((action, _)) = Self::toolbar_buttons(width)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
        else {
            return false;
        };
        match action {
            ToolbarAction::Undo => self.undo(),
            ToolbarAction::Redo => self.redo(),
            ToolbarAction::ZoomIn => {
                let (cx, cy) = self.main_rect(width, height).center();
                self.set_zoom_about(width, height, self.zoom * 1.25, cx, cy);
            }
            ToolbarAction::ZoomOut => {
                let (cx, cy) = self.main_rect(width, height).center();
                self.set_zoom_about(width, height, self.zoom / 1.25, cx, cy);
            }
            ToolbarAction::Fit => self.fit(),
            ToolbarAction::Grid => self.cycle_grid(),
            ToolbarAction::Snap => self.toggle_snap(),
            ToolbarAction::UiScale => {
                self.settings.cycle_ui_scale();
                self.font.ui_scale = self.settings.ui_scale;
                if self.persist_settings() {
                    self.status =
                        format!("Interface text · {:.0}%", self.settings.ui_scale * 100.0);
                }
            }
            ToolbarAction::HighlightChanges => {
                self.settings.highlight_changes = !self.settings.highlight_changes;
                if !self.settings.highlight_changes {
                    self.highlighted_change = None;
                }
                if self.persist_settings() {
                    self.status = format!(
                        "Highlight changes · {}",
                        if self.settings.highlight_changes {
                            "on"
                        } else {
                            "off"
                        }
                    );
                }
            }
            ToolbarAction::FollowChanges => {
                self.settings.follow_changes = !self.settings.follow_changes;
                if self.persist_settings() {
                    self.status = format!(
                        "Follow external changes · {}",
                        if self.settings.follow_changes {
                            "on"
                        } else {
                            "off"
                        }
                    );
                }
            }
            ToolbarAction::TextSelect => {
                self.text_select_mode = !self.text_select_mode;
                self.text_drag = None;
                if !self.text_select_mode {
                    self.text_selection = None;
                }
                self.status = if self.text_select_mode {
                    "Text selection · drag text, then Ctrl/Command C to copy".to_owned()
                } else {
                    self.live_status()
                };
            }
            ToolbarAction::Theme => self.toggle_theme(),
        }
        true
    }

    fn persist_settings(&mut self) -> bool {
        if let Err(error) = self.settings.save() {
            self.status = format!("Could not save viewer settings: {error:#}");
            false
        } else {
            true
        }
    }

    fn cycle_grid(&mut self) {
        const GRID_STEPS: &[f64] = &[0.254, 0.508, 1.27, 2.54, 5.08];
        let index = GRID_STEPS
            .iter()
            .position(|step| (*step - self.grid_mm).abs() < 1e-9)
            .unwrap_or(2);
        self.grid_mm = GRID_STEPS[(index + 1) % GRID_STEPS.len()];
        self.status = format!("Grid · {:.3} mm", self.grid_mm);
    }

    fn toggle_snap(&mut self) {
        self.snap_enabled = !self.snap_enabled;
        self.status = format!("Snap · {}", if self.snap_enabled { "on" } else { "off" });
    }

    fn start_search(&mut self) {
        self.search = Some(SearchState::default());
        self.status = "Search · type a reference, value, net, UUID, or sheet".to_owned();
    }

    fn refresh_search(&mut self) {
        let Some(search) = &mut self.search else {
            return;
        };
        search.hits.clear();
        search.current = 0;
        let query = search.query.trim().to_lowercase();
        if query.is_empty() {
            return;
        }
        for (sheet_index, sheet) in self.sheets.iter().enumerate() {
            let sheet_text = format!("{} {}", sheet.name, sheet.file.display()).to_lowercase();
            if sheet_text.contains(&query) {
                search.hits.push(SearchHit {
                    sheet: sheet_index,
                    uuid: None,
                    description: format!("Sheet · {}", sheet.name),
                });
            }
            for object in &sheet.semantic.objects {
                if object.search_text.to_lowercase().contains(&query) {
                    search.hits.push(SearchHit {
                        sheet: sheet_index,
                        uuid: Some(object.uuid.clone()),
                        description: format!("{:?} · {}", object.kind, object.label),
                    });
                }
            }
        }
        self.status = format!("Search · {} result(s)", search.hits.len());
    }

    fn activate_search_hit(&mut self) {
        let (hit, index, hit_count) = {
            let Some(search) = &mut self.search else {
                return;
            };
            if search.hits.is_empty() {
                self.status = format!("Search · no results for ‘{}’", search.query);
                return;
            }
            let index = search.current.min(search.hits.len() - 1);
            let hit = search.hits[index].clone();
            let hit_count = search.hits.len();
            search.current = (index + 1) % hit_count;
            (hit, index, hit_count)
        };
        self.active = hit.sheet;
        self.fit();
        self.selected_uuids.clear();
        if let Some(uuid) = hit.uuid {
            self.selected_uuids.insert(uuid);
        }
        self.status = format!(
            "Search result {}/{} · {}",
            index + 1,
            hit_count,
            hit.description
        );
    }

    fn handle_search_key(&mut self, key: Key<&str>) -> bool {
        if self.search.is_none() {
            return false;
        }
        match key {
            Key::Named(NamedKey::Escape) => {
                self.search = None;
                self.status = self.live_status();
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(search) = &mut self.search {
                    search.query.pop();
                }
                self.refresh_search();
            }
            Key::Named(NamedKey::Enter) => self.activate_search_hit(),
            Key::Character(value)
                if !self.modifiers.control_key() && !self.modifiers.super_key() =>
            {
                if let Some(search) = &mut self.search {
                    search.query.push_str(value);
                }
                self.refresh_search();
            }
            _ => {}
        }
        true
    }

    fn start_property_edit(&mut self) {
        if !self.require_edit_mode() {
            return;
        }
        if self.selected_uuids.len() != 1 {
            self.status = "Select exactly one symbol to edit its properties".to_owned();
            return;
        }
        let Some(uuid) = self.selected_uuids.iter().next() else {
            return;
        };
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let Some(object) = sheet
            .semantic
            .objects
            .iter()
            .find(|object| &object.uuid == uuid)
        else {
            return;
        };
        let property = object
            .properties
            .iter()
            .find(|property| property.name == "Value")
            .or_else(|| {
                object.properties.iter().find(|property| {
                    !matches!(
                        property.name.as_str(),
                        "Library" | "Position" | "Reference" | "Text"
                    )
                })
            });
        let Some(property) = property else {
            self.status = "This item has no directly editable property yet".to_owned();
            return;
        };
        self.property_edit = Some(PropertyEdit {
            file: sheet.file.clone(),
            uuid: uuid.clone(),
            name: property.name.clone(),
            value: property.value.clone(),
        });
        self.status = format!("Editing {} · Enter to stage · Esc to cancel", property.name);
    }

    fn commit_property_edit(&mut self) {
        let Some(edit) = self.property_edit.take() else {
            return;
        };
        let Some(sheet) = self
            .sheets
            .iter()
            .find(|sheet| path_key(&sheet.file) == path_key(&edit.file))
        else {
            self.status = "Property edit rejected: sheet is no longer loaded".to_owned();
            return;
        };
        if sheet
            .semantic
            .objects
            .iter()
            .find(|object| object.uuid == edit.uuid)
            .and_then(|object| {
                object
                    .properties
                    .iter()
                    .find(|property| property.name == edit.name)
            })
            .is_some_and(|property| property.value == edit.value)
        {
            self.status = format!("{} is unchanged", edit.name);
            return;
        }
        let id = match ItemId::new(edit.uuid.clone()) {
            Ok(id) => id,
            Err(error) => {
                self.status = format!("Property edit rejected: {error}");
                return;
            }
        };
        let command = match SchematicCommand::set_property(
            &sheet.semantic.source,
            id,
            &edit.name,
            &edit.value,
            format!("Edit {}", edit.name),
        ) {
            Ok(command) => command,
            Err(error) => {
                self.status = format!("Property edit rejected: {error}");
                return;
            }
        };
        match self.stage_command(&edit.file, &command) {
            Ok(outcome) => {
                self.record_command_change(ChangeOrigin::Local, &edit.file, &command);
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(edit.file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.status = format!("Staged {} · press Commit to write", edit.name);
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.status = format!("Property staging conflict: {error}");
            }
            Err(error) => self.status = format!("Could not stage property: {error}"),
        }
    }

    fn handle_property_key(&mut self, key: Key<&str>) -> bool {
        if self.property_edit.is_none() {
            return false;
        }
        match key {
            Key::Named(NamedKey::Escape) => {
                self.property_edit = None;
                self.status = self.live_status();
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(edit) = &mut self.property_edit {
                    edit.value.pop();
                }
            }
            Key::Named(NamedKey::Enter) => self.commit_property_edit(),
            Key::Character(value)
                if !self.modifiers.control_key() && !self.modifiers.super_key() =>
            {
                if let Some(edit) = &mut self.property_edit {
                    edit.value.push_str(value);
                }
            }
            _ => {}
        }
        true
    }

    fn thumbnail_rect(&self, index: usize, height: f64) -> ScreenRect {
        let x0 = SIDEBAR_WIDTH + THUMBNAIL_GAP + index as f64 * (THUMBNAIL_WIDTH + THUMBNAIL_GAP)
            - self.film_scroll;
        ScreenRect {
            x0,
            y0: height - FILMSTRIP_HEIGHT + 9.0,
            x1: x0 + THUMBNAIL_WIDTH,
            y1: height - TIMELINE_HEIGHT - 9.0,
        }
    }

    fn handle_filmstrip(&mut self, width: f64, height: f64, x: f64, y: f64) -> bool {
        if !self.filmstrip_rect(width, height).contains(x, y) {
            return false;
        }
        if y >= height - TIMELINE_HEIGHT {
            if let Some((id, _)) = self
                .timeline_event_rects(width, height)
                .into_iter()
                .find(|(_, rect)| rect.contains(x, y))
            {
                self.navigate_to_change(id);
            }
            return true;
        }
        if let Some(index) =
            (0..self.sheets.len()).find(|index| self.thumbnail_rect(*index, height).contains(x, y))
        {
            self.switch_sheet(index);
        }
        true
    }

    fn timeline_event_rects(&self, width: f64, height: f64) -> Vec<(u64, ScreenRect)> {
        const LABEL_WIDTH: f64 = 96.0;
        const CARD_WIDTH: f64 = 138.0;
        const CARD_GAP: f64 = 6.0;

        let available = (width - SIDEBAR_WIDTH - LABEL_WIDTH - 20.0).max(CARD_WIDTH);
        let visible = ((available + CARD_GAP) / (CARD_WIDTH + CARD_GAP))
            .floor()
            .max(1.0) as usize;
        let mut ids = self
            .timeline
            .events()
            .rev()
            .skip(self.timeline_scroll)
            .take(visible)
            .map(|event| event.id)
            .collect::<Vec<_>>();
        ids.reverse();
        ids.into_iter()
            .enumerate()
            .map(|(index, id)| {
                let x0 = SIDEBAR_WIDTH + LABEL_WIDTH + index as f64 * (CARD_WIDTH + CARD_GAP);
                (
                    id,
                    ScreenRect {
                        x0,
                        y0: height - TIMELINE_HEIGHT + 7.0,
                        x1: x0 + CARD_WIDTH,
                        y1: height - 7.0,
                    },
                )
            })
            .collect()
    }

    fn draw_timeline(&mut self, width: f64, height: f64, palette: Palette) {
        let y0 = height - TIMELINE_HEIGHT;
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.toolbar.with_alpha(0.98),
            None,
            &Rect::new(SIDEBAR_WIDTH, y0, width, height),
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &format!("CHANGES · {}", self.timeline.len()),
            TextRun {
                size: 9.5,
                position: (SIDEBAR_WIDTH + 10.0, y0 + 17.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.accent,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            if self.timeline.len() == 0 {
                "Session history"
            } else if self.timeline_scroll == 0 {
                "Newest · scroll"
            } else {
                "Earlier · scroll"
            },
            TextRun {
                size: 8.0,
                position: (SIDEBAR_WIDTH + 10.0, y0 + 34.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text.with_alpha(0.75),
            },
        );
        if self.timeline.len() == 0 {
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                "No changes yet — edits and external updates appear here",
                TextRun {
                    size: 9.5,
                    position: (SIDEBAR_WIDTH + 106.0, y0 + 25.0),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: palette.text.with_alpha(0.62),
                },
            );
        }
        let now = Instant::now();
        for (id, rect) in self.timeline_event_rects(width, height) {
            let Some(event) = self.timeline.event(id) else {
                continue;
            };
            let progress = event.intro_progress(now);
            let eased = 1.0 - (1.0 - progress).powi(3);
            let dy = (1.0 - eased) * 12.0;
            let card = RoundedRect::new(rect.x0, rect.y0 + dy, rect.x1, rect.y1 + dy, 6.0);
            let origin_color = match event.origin {
                ChangeOrigin::External => palette.bus,
                ChangeOrigin::Local => palette.accent,
                ChangeOrigin::Undo => palette.selection,
                ChangeOrigin::Redo => palette.junction,
            };
            self.frame.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                palette.card.with_alpha((0.72 + eased * 0.28) as f32),
                None,
                &card,
            );
            self.frame.stroke(
                &Stroke::new(if self.highlighted_change == Some(id) {
                    2.5
                } else {
                    1.0
                }),
                Affine::IDENTITY,
                if self.highlighted_change == Some(id) {
                    origin_color
                } else {
                    palette.card_border
                },
                None,
                &card,
            );
            let icon_rect = ScreenRect {
                x0: rect.x0 + 7.0,
                y0: rect.y0 + 7.0 + dy,
                x1: rect.x0 + 29.0,
                y1: rect.y1 - 7.0 + dy,
            };
            self.frame.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                origin_color.with_alpha(0.14),
                None,
                &RoundedRect::new(icon_rect.x0, icon_rect.y0, icon_rect.x1, icon_rect.y1, 5.0),
            );
            draw_ui_icon(
                &mut self.frame,
                change_icon(event.kind),
                icon_rect,
                origin_color,
            );
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &truncate_ui(&event.label, 19),
                TextRun {
                    size: 9.0,
                    position: (rect.x0 + 35.0, rect.y0 + 12.0 + dy),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: palette.text,
                },
            );
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &format!(
                    "{} · {}",
                    event.origin.label(),
                    relative_time(event.recorded_at)
                ),
                TextRun {
                    size: 7.5,
                    position: (rect.x0 + 35.0, rect.y0 + 27.0 + dy),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: origin_color,
                },
            );
        }
    }

    fn select_at(&mut self, width: f64, height: f64, x: f64, y: f64, additive: bool) -> bool {
        let Some(point) = self.schematic_point(width, height, x, y) else {
            return false;
        };
        let Some(sheet) = self.sheets.get(self.active) else {
            return false;
        };
        let tolerance = 5.0
            / self
                .page_transform(width, height)
                .map(|(_, scale, _, _)| scale)
                .unwrap_or(1.0);
        if let Some(object) = sheet.semantic.hit_test(point, tolerance) {
            if additive && !self.selected_uuids.insert(object.uuid.clone()) {
                self.selected_uuids.remove(&object.uuid);
            } else if !additive {
                self.selected_uuids.clear();
                self.selected_uuids.insert(object.uuid.clone());
            }
            self.status = format!(
                "Selected {} item(s) · {:?} · {}",
                self.selected_uuids.len(),
                object.kind,
                object.label
            );
            true
        } else {
            if !additive {
                self.selected_uuids.clear();
            }
            false
        }
    }

    fn finish_box_selection(&mut self, width: f64, height: f64, selection: SelectionBox) {
        let Some(start) = self.schematic_point(width, height, selection.start.x, selection.start.y)
        else {
            return;
        };
        let Some(end) =
            self.schematic_point(width, height, selection.current.x, selection.current.y)
        else {
            return;
        };
        if !selection.additive {
            self.selected_uuids.clear();
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        for object in &sheet.semantic.objects {
            if box_selects_bounds(object.bounds, start, end) {
                self.selected_uuids.insert(object.uuid.clone());
            }
        }
        self.status = format!("Selected {} item(s)", self.selected_uuids.len());
    }

    fn selected_items_are_movable(&self) -> bool {
        !self.selected_uuids.is_empty()
            && self.sheets.get(self.active).is_some_and(|sheet| {
                self.selected_uuids.iter().all(|uuid| {
                    sheet
                        .semantic
                        .objects
                        .iter()
                        .any(|object| &object.uuid == uuid)
                })
            })
    }

    fn drag_delta(&self, width: f64, height: f64, drag: &ItemDrag) -> Option<(f64, f64)> {
        let (_, scale, _, _) = self.page_transform(width, height)?;
        Some(drag_delta_mm(
            drag.start,
            drag.current,
            scale,
            self.snap_enabled,
            self.grid_mm,
        ))
    }

    fn finish_item_drag(&mut self, width: f64, height: f64, drag: ItemDrag) {
        let distance = (drag.current.x - drag.start.x).hypot(drag.current.y - drag.start.y);
        if distance < 2.0 {
            return;
        }
        let Some((dx, dy)) = self.drag_delta(width, height, &drag) else {
            return;
        };
        if dx.abs() < 1e-9 && dy.abs() < 1e-9 {
            self.status = "Drag snapped back to the original position".to_owned();
            return;
        }
        self.nudge_selected(dx, dy);
    }

    fn start_item_drag(&mut self, cursor: PhysicalPosition<f64>) {
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let connected_wires =
            match connected_wire_moves(&sheet.semantic.source, &self.selected_uuids) {
                Ok(wires) => wires,
                Err(error) => {
                    self.status = format!("Could not prepare connected drag: {error:#}");
                    return;
                }
            };
        let mut excluded_uuids = self.selected_uuids.clone();
        excluded_uuids.extend(connected_wires.iter().map(|wire| wire.uuid.clone()));
        let excluded = sheet
            .semantic
            .objects
            .iter()
            .filter(|object| excluded_uuids.contains(&object.uuid))
            .map(|object| object.primitive_range.clone())
            .collect::<Vec<_>>();
        let base_scene = encode_scene_without_ranges(&sheet.semantic, &excluded, self.palette());
        self.item_drag = Some(ItemDrag {
            start: cursor,
            current: cursor,
            base_scene,
            connected_wires,
        });
    }

    fn nudge_selected(&mut self, dx: f64, dy: f64) {
        if !self.require_edit_mode() {
            return;
        }
        if self.selected_uuids.is_empty() {
            self.status = "Select a symbol before using Ctrl/Command + Arrow".to_owned();
            return;
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let mut uuids = self.selected_uuids.iter().cloned().collect::<Vec<_>>();
        uuids.sort();
        let selected_objects = uuids
            .iter()
            .filter_map(|uuid| {
                sheet
                    .semantic
                    .objects
                    .iter()
                    .find(|object| &object.uuid == uuid)
            })
            .collect::<Vec<_>>();
        if selected_objects.len() != uuids.len() {
            self.status = "Part of the selection is no longer present; reloading".to_owned();
            return;
        }
        let file = sheet.semantic.file.clone();
        let expected = sheet.semantic.source.clone();
        let (edited, changed_uuids) =
            match move_items_with_connected_wires(&expected, &self.selected_uuids, dx, dy) {
                Ok(result) => result,
                Err(error) => {
                    self.status = format!("Edit rejected: {error:#}");
                    return;
                }
            };
        let item_ids = match changed_uuids
            .iter()
            .cloned()
            .map(ItemId::new)
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(item_ids) => item_ids,
            Err(error) => {
                self.status = format!("Edit rejected: {error}");
                return;
            }
        };
        let command = match SchematicCommand::replace_items_from_document(
            &expected,
            &edited,
            item_ids,
            if uuids.len() == 1 {
                "Move item"
            } else {
                "Move items"
            },
        ) {
            Ok(command) => command,
            Err(error) => {
                self.status = format!("Edit rejected: {error}");
                return;
            }
        };
        match self.stage_command(&file, &command) {
            Ok(outcome) => {
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.record_change(
                    ChangeOrigin::Local,
                    format!("Moved {} item(s)", uuids.len()),
                    file.clone(),
                    changed_uuids,
                );
                let rebased = if outcome.rebased {
                    " · safely rebased"
                } else {
                    ""
                };
                self.status = format!(
                    "Staged{rebased} · moved {} item(s) by ({dx:.2}, {dy:.2}) mm · press Commit",
                    uuids.len()
                );
            }
            Err(SexpError::Conflict { .. } | SexpError::ItemConflict { .. }) => {
                self.status =
                    "Staging conflict: the selected item changed; no durable file was written"
                        .to_owned();
            }
            Err(error) => {
                self.status = format!("Could not stage edit: {error}");
            }
        }
    }

    fn delete_selected(&mut self) {
        if !self.require_edit_mode() {
            return;
        }
        if self.selected_uuids.is_empty() {
            self.status = "Select one or more items to stage for deletion".to_owned();
            return;
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let file = sheet.file.clone();
        let ids = match self
            .selected_uuids
            .iter()
            .cloned()
            .map(ItemId::new)
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(ids) => ids,
            Err(error) => {
                self.status = format!("Delete rejected: {error}");
                return;
            }
        };
        let count = ids.len();
        let command = match SchematicCommand::delete_items(
            &sheet.semantic.source,
            ids,
            if count == 1 {
                "Delete item"
            } else {
                "Delete items"
            },
        ) {
            Ok(command) => command,
            Err(error) => {
                self.status = format!("Delete rejected: {error}");
                return;
            }
        };
        match self.stage_command(&file, &command) {
            Ok(outcome) => {
                self.record_command_change(ChangeOrigin::Local, &file, &command);
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.selected_uuids.clear();
                self.status = format!(
                    "Staged deletion of {count} item(s) · Undo restores them · press Commit"
                );
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.status = format!("Delete staging conflict: {error}");
            }
            Err(error) => self.status = format!("Could not stage deletion: {error}"),
        }
    }

    fn transform_selected_items(
        &mut self,
        operation: &str,
        past_tense: &str,
        item_name: &str,
        supports: impl Fn(ObjectKind) -> bool,
        transform: impl Fn(&str, &str) -> Result<String>,
    ) {
        if !self.require_edit_mode() {
            return;
        }
        if self.selected_uuids.is_empty() {
            self.status = format!("Select one or more items to {}", operation.to_lowercase());
            return;
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let mut uuids = self.selected_uuids.iter().cloned().collect::<Vec<_>>();
        uuids.sort();
        if !uuids.iter().all(|uuid| {
            sheet
                .semantic
                .objects
                .iter()
                .any(|object| &object.uuid == uuid && supports(object.kind))
        }) {
            self.status = format!("{operation} requires {item_name}");
            return;
        }
        let file = sheet.file.clone();
        let expected = Arc::clone(&sheet.semantic.source);
        let mut edited = expected.to_string();
        for uuid in &uuids {
            edited = match transform(&edited, uuid) {
                Ok(edited) => edited,
                Err(error) => {
                    self.status = format!("{operation} rejected: {error:#}");
                    return;
                }
            };
        }
        let ids = match uuids
            .iter()
            .cloned()
            .map(ItemId::new)
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(ids) => ids,
            Err(error) => {
                self.status = format!("{operation} rejected: {error}");
                return;
            }
        };
        let command = match SchematicCommand::replace_items_from_document(
            &expected,
            &edited,
            ids,
            format!("{operation} {item_name}"),
        ) {
            Ok(command) => command,
            Err(error) => {
                self.status = format!("{operation} rejected: {error}");
                return;
            }
        };
        match self.stage_command(&file, &command) {
            Ok(outcome) => {
                self.record_command_change(ChangeOrigin::Local, &file, &command);
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.status = format!(
                    "Staged · {past_tense} {} {item_name} · press Commit",
                    uuids.len()
                );
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.status = format!("{operation} staging conflict: {error}");
            }
            Err(error) => self.status = format!("Could not stage {operation}: {error}"),
        }
    }

    fn rotate_selected(&mut self) {
        self.transform_selected_items(
            "Rotate",
            "Rotated",
            "symbol(s) or bus entry/entries",
            |kind| matches!(kind, ObjectKind::Symbol | ObjectKind::BusEntry),
            rotate_item_source,
        );
    }

    fn mirror_selected(&mut self, axis: &'static str) {
        self.transform_selected_items(
            "Mirror",
            "Mirrored",
            "symbol(s)",
            |kind| kind == ObjectKind::Symbol,
            move |source, uuid| crate::native_scene::mirror_symbol_source(source, uuid, axis),
        );
    }

    fn duplicate_selected(&mut self) {
        if !self.require_edit_mode() {
            return;
        }
        if self.selected_uuids.len() != 1 {
            self.status = "Select exactly one symbol to duplicate".to_owned();
            return;
        }
        let Some(uuid) = self.selected_uuids.iter().next().cloned() else {
            return;
        };
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        if !sheet
            .semantic
            .objects
            .iter()
            .any(|object| object.uuid == uuid && object.kind == ObjectKind::Symbol)
        {
            self.status = "Duplicate currently supports placed symbols".to_owned();
            return;
        }
        let file = sheet.file.clone();
        let (duplicate, duplicate_uuid) =
            match duplicate_symbol_block(&sheet.semantic.source, &uuid, self.grid_mm, self.grid_mm)
            {
                Ok(duplicate) => duplicate,
                Err(error) => {
                    self.status = format!("Duplicate rejected: {error:#}");
                    return;
                }
            };
        let command = match SchematicCommand::insert_item(
            &sheet.semantic.source,
            duplicate,
            ItemAnchor::BeforeFooter,
            "Duplicate symbol",
        ) {
            Ok(command) => command,
            Err(error) => {
                self.status = format!("Duplicate rejected: {error}");
                return;
            }
        };
        match self.stage_command(&file, &command) {
            Ok(outcome) => {
                self.record_command_change(ChangeOrigin::Local, &file, &command);
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.selected_uuids.clear();
                self.selected_uuids.insert(duplicate_uuid);
                self.status =
                    "Staged duplicate · reference left unannotated · press Commit".to_owned();
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.status = format!("Duplicate staging conflict: {error}");
            }
            Err(error) => self.status = format!("Could not stage duplicate: {error}"),
        }
    }

    fn snap_schematic_point(&self, point: SchPoint) -> SchPoint {
        snap_point(point, self.snap_enabled, self.grid_mm)
    }

    fn start_wire(&mut self, width: f64, height: f64, is_bus: bool) {
        if !self.require_edit_mode() {
            return;
        }
        let Some(cursor) = self.cursor else {
            self.status = "Move the pointer onto the sheet before starting a wire".to_owned();
            return;
        };
        if !self.main_rect(width, height).contains(cursor.x, cursor.y) {
            self.status = "Start wires inside the schematic page".to_owned();
            return;
        }
        let Some(point) = self.schematic_point(width, height, cursor.x, cursor.y) else {
            return;
        };
        let point = self.snap_schematic_point(point);
        self.wire_draft = Some(WireDraft {
            start: point,
            current: point,
            is_bus,
        });
        let kind = if is_bus { "Bus" } else { "Wire" };
        self.status = format!("{kind} · click endpoint to stage · Esc to cancel");
    }

    fn update_wire_draft(&mut self, width: f64, height: f64, cursor: PhysicalPosition<f64>) {
        let Some(point) = self.schematic_point(width, height, cursor.x, cursor.y) else {
            return;
        };
        let point = self.snap_schematic_point(point);
        if let Some(wire) = &mut self.wire_draft {
            wire.current = point;
        }
    }

    fn commit_wire(&mut self) {
        let Some(wire) = self.wire_draft.take() else {
            return;
        };
        if (wire.current.x - wire.start.x).hypot(wire.current.y - wire.start.y) < 1e-9 {
            let kind = if wire.is_bus { "Bus" } else { "Wire" };
            self.status = format!("{kind} needs two distinct endpoints");
            return;
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let file = sheet.file.clone();
        let source = Arc::clone(&sheet.semantic.source);
        let (block, kind, display_kind) = if wire.is_bus {
            (
                konnect_sexp::schematic::format_bus(
                    wire.start.x,
                    wire.start.y,
                    wire.current.x,
                    wire.current.y,
                ),
                "bus",
                "Bus",
            )
        } else {
            (
                konnect_sexp::schematic::format_wire(
                    wire.start.x,
                    wire.start.y,
                    wire.current.x,
                    wire.current.y,
                ),
                "wire",
                "Wire",
            )
        };
        let command = match SchematicCommand::insert_item(
            &source,
            block,
            ItemAnchor::BeforeFooter,
            format!("Add {kind}"),
        ) {
            Ok(command) => command,
            Err(error) => {
                self.status = format!("{display_kind} rejected: {error}");
                return;
            }
        };
        match self.stage_command(&file, &command) {
            Ok(outcome) => {
                self.record_command_change(ChangeOrigin::Local, &file, &command);
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.status = format!("Staged {kind} · press Commit to write");
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.status = format!("{display_kind} staging conflict: {error}");
            }
            Err(error) => self.status = format!("Could not stage {kind}: {error}"),
        }
    }

    fn cursor_schematic_point(&self, width: f64, height: f64) -> Option<SchPoint> {
        let cursor = self.cursor?;
        if !self.main_rect(width, height).contains(cursor.x, cursor.y) {
            return None;
        }
        self.schematic_point(width, height, cursor.x, cursor.y)
            .map(|point| self.snap_schematic_point(point))
    }

    fn insert_at_cursor(
        &mut self,
        width: f64,
        height: f64,
        description: &'static str,
        format: impl FnOnce(SchPoint) -> String,
    ) {
        let Some(point) = self.cursor_schematic_point(width, height) else {
            self.status = format!("Place {description} inside the schematic page");
            return;
        };
        self.commit_insert(format(point), description);
    }

    fn commit_insert(&mut self, block: String, description: &str) {
        if !self.require_edit_mode() {
            return;
        }
        let Some((file, source)) = self
            .sheets
            .get(self.active)
            .map(|sheet| (sheet.file.clone(), Arc::clone(&sheet.semantic.source)))
        else {
            return;
        };
        let command = match SchematicCommand::insert_item(
            &source,
            block,
            ItemAnchor::BeforeFooter,
            format!("Add {description}"),
        ) {
            Ok(command) => command,
            Err(error) => {
                self.status = format!("{description} rejected: {error}");
                return;
            }
        };
        match self.stage_command(&file, &command) {
            Ok(outcome) => {
                self.record_command_change(ChangeOrigin::Local, &file, &command);
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.status = format!("Staged {description} · press Commit to write");
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.status = format!("{description} staging conflict: {error}");
            }
            Err(error) => self.status = format!("Could not stage {description}: {error}"),
        }
    }

    fn start_label_edit(&mut self, width: f64, height: f64) {
        if !self.require_edit_mode() {
            return;
        }
        let Some(point) = self.cursor_schematic_point(width, height) else {
            self.status = "Place labels inside the schematic page".to_owned();
            return;
        };
        self.label_edit = Some(LabelEdit {
            point,
            value: String::new(),
        });
        self.status = "New local label · enter a net name · Enter to place".to_owned();
    }

    fn commit_label_edit(&mut self) {
        let Some(edit) = self.label_edit.take() else {
            return;
        };
        let value = edit.value.trim();
        if value.is_empty() {
            self.label_edit = Some(edit);
            self.status = "A label needs a non-empty net name".to_owned();
            return;
        }
        self.commit_insert(
            konnect_sexp::schematic::format_net_label(value, edit.point.x, edit.point.y, 0.0),
            "local label",
        );
    }

    fn handle_label_key(&mut self, key: Key<&str>) -> bool {
        if self.label_edit.is_none() {
            return false;
        }
        match key {
            Key::Named(NamedKey::Escape) => {
                self.label_edit = None;
                self.status = self.live_status();
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(edit) = &mut self.label_edit {
                    edit.value.pop();
                }
            }
            Key::Named(NamedKey::Enter) => self.commit_label_edit(),
            Key::Character(value)
                if !self.modifiers.control_key() && !self.modifiers.super_key() =>
            {
                if let Some(edit) = &mut self.label_edit {
                    edit.value.push_str(value);
                }
            }
            _ => {}
        }
        true
    }

    fn start_sheet_edit(&mut self, width: f64, height: f64) {
        if !self.require_edit_mode() {
            return;
        }
        let Some(mut point) = self.cursor_schematic_point(width, height) else {
            self.status = "Place hierarchical sheets inside the schematic page".to_owned();
            return;
        };
        if let Some(sheet) = self.sheets.get(self.active) {
            point.x = point
                .x
                .clamp(2.54, (sheet.semantic.width_mm - 82.54).max(2.54));
            point.y = point
                .y
                .clamp(2.54, (sheet.semantic.height_mm - 52.54).max(2.54));
        }
        self.sheet_edit = Some(SheetEdit {
            point,
            name: String::new(),
            file: String::new(),
            field: SheetEditField::Name,
        });
        self.status = "New hierarchical sheet · Tab switches name/file · Enter creates".to_owned();
    }

    fn handle_sheet_key(&mut self, key: Key<&str>) -> bool {
        if self.sheet_edit.is_none() {
            return false;
        }
        match key {
            Key::Named(NamedKey::Escape) => {
                self.sheet_edit = None;
                self.status = self.live_status();
            }
            Key::Named(NamedKey::Tab) => {
                if let Some(edit) = &mut self.sheet_edit {
                    edit.field = match edit.field {
                        SheetEditField::Name => SheetEditField::File,
                        SheetEditField::File => SheetEditField::Name,
                    };
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(edit) = &mut self.sheet_edit {
                    match edit.field {
                        SheetEditField::Name => {
                            edit.name.pop();
                        }
                        SheetEditField::File => {
                            edit.file.pop();
                        }
                    }
                }
            }
            Key::Named(NamedKey::Enter) => self.commit_sheet_edit(),
            Key::Character(value)
                if !self.modifiers.control_key() && !self.modifiers.super_key() =>
            {
                if let Some(edit) = &mut self.sheet_edit {
                    match edit.field {
                        SheetEditField::Name => edit.name.push_str(value),
                        SheetEditField::File => edit.file.push_str(value),
                    }
                }
            }
            _ => {}
        }
        true
    }

    fn start_sheet_pin_edit(&mut self, width: f64, height: f64) {
        if !self.require_edit_mode() {
            return;
        }
        let Some(sheet_uuid) = self
            .selected_uuids
            .iter()
            .next()
            .filter(|_| self.selected_uuids.len() == 1)
            .cloned()
        else {
            self.status = "Select exactly one hierarchical sheet before adding a pin".to_owned();
            return;
        };
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        if !sheet
            .semantic
            .objects
            .iter()
            .any(|object| object.uuid == sheet_uuid && object.kind == ObjectKind::Sheet)
        {
            self.status = "The selected item is not a hierarchical sheet".to_owned();
            return;
        }
        let Some(rectangle) = sheet_rectangle(&sheet.semantic.source, &sheet_uuid) else {
            self.status = "Could not read the selected sheet geometry".to_owned();
            return;
        };
        let cursor = self
            .cursor_schematic_point(width, height)
            .unwrap_or(SchPoint {
                x: rectangle.2,
                y: (rectangle.1 + rectangle.3) / 2.0,
            });
        let (point, rotation) =
            nearest_sheet_edge(rectangle, cursor, self.grid_mm, self.snap_enabled);
        self.sheet_pin_edit = Some(SheetPinEdit {
            sheet_uuid,
            point,
            rotation,
            name: String::new(),
            pin_type: SheetPinType::Passive,
        });
        self.status = "New sheet pin · Tab cycles electrical type · Enter places".to_owned();
    }

    fn handle_sheet_pin_key(&mut self, key: Key<&str>) -> bool {
        if self.sheet_pin_edit.is_none() {
            return false;
        }
        match key {
            Key::Named(NamedKey::Escape) => {
                self.sheet_pin_edit = None;
                self.status = self.live_status();
            }
            Key::Named(NamedKey::Tab) => {
                if let Some(edit) = &mut self.sheet_pin_edit {
                    edit.pin_type = edit.pin_type.next();
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(edit) = &mut self.sheet_pin_edit {
                    edit.name.pop();
                }
            }
            Key::Named(NamedKey::Enter) => self.commit_sheet_pin_edit(),
            Key::Character(value)
                if !self.modifiers.control_key() && !self.modifiers.super_key() =>
            {
                if let Some(edit) = &mut self.sheet_pin_edit {
                    edit.name.push_str(value);
                }
            }
            _ => {}
        }
        true
    }

    fn commit_sheet_pin_edit(&mut self) {
        let Some(edit) = self.sheet_pin_edit.take() else {
            return;
        };
        let name = edit.name.trim().to_owned();
        if name.is_empty() {
            self.sheet_pin_edit = Some(edit);
            self.status = "A hierarchical sheet pin needs a non-empty name".to_owned();
            return;
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        let file = sheet.file.clone();
        let source = Arc::clone(&sheet.semantic.source);
        let pin = konnect_sexp::schematic::format_sheet_pin(
            &name,
            edit.pin_type,
            edit.point.x,
            edit.point.y,
            edit.rotation,
        );
        let command = match SchematicCommand::insert_sheet_pin(
            &source,
            match ItemId::new(edit.sheet_uuid.clone()) {
                Ok(id) => id,
                Err(error) => {
                    self.status = format!("Sheet pin rejected: {error}");
                    return;
                }
            },
            &pin,
            "Add hierarchical sheet pin",
        ) {
            Ok(command) => command,
            Err(error) => {
                self.sheet_pin_edit = Some(edit);
                self.status = format!("Sheet pin rejected: {error}");
                return;
            }
        };
        match self.stage_command(&file, &command) {
            Ok(outcome) => {
                self.record_command_change(ChangeOrigin::Local, &file, &command);
                push_history(
                    &mut self.undo_stack,
                    HistoryEntry::single(file.clone(), outcome.inverse),
                );
                self.redo_stack.clear();
                self.status = format!(
                    "Staged {name} {} sheet pin · press Commit",
                    edit.pin_type.keyword()
                );
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.sheet_pin_edit = Some(edit);
                self.status = format!("Sheet-pin staging conflict: {error}");
            }
            Err(error) => {
                self.sheet_pin_edit = Some(edit);
                self.status = format!("Could not stage sheet pin: {error}");
            }
        }
    }

    fn commit_sheet_edit(&mut self) {
        let Some(edit) = self.sheet_edit.take() else {
            return;
        };
        let name = edit.name.trim().to_owned();
        let file_name = edit.file.trim().to_owned();
        if name.is_empty() || file_name.is_empty() {
            self.sheet_edit = Some(edit);
            self.status = "Hierarchical sheet needs both a name and child filename".to_owned();
            return;
        }
        let relative = Path::new(&file_name);
        let valid_relative = !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
            && relative
                .extension()
                .is_some_and(|extension| extension == "kicad_sch");
        if !valid_relative {
            self.sheet_edit = Some(edit);
            self.status =
                "Child file must be a relative .kicad_sch path without parent traversal".to_owned();
            return;
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        if sheet.semantic.objects.iter().any(|object| {
            object.kind == ObjectKind::Sheet
                && object
                    .properties
                    .iter()
                    .any(|property| property.name == "Sheetname" && property.value == name)
        }) {
            self.sheet_edit = Some(edit);
            self.status = format!("A hierarchical sheet named ‘{name}’ already exists");
            return;
        }
        let parent_file = sheet.file.clone();
        let source = Arc::clone(&sheet.semantic.source);
        let Some(parent_dir) = parent_file.parent().map(Path::to_path_buf) else {
            self.status = "Parent schematic has no directory".to_owned();
            return;
        };
        let child_file = parent_dir.join(relative);
        if !child_file.parent().is_some_and(Path::is_dir) {
            self.sheet_edit = Some(edit);
            self.status = "The child sheet directory does not exist".to_owned();
            return;
        }
        if path_key(&child_file) == path_key(&parent_file) {
            self.sheet_edit = Some(edit);
            self.status = "A sheet cannot reference itself".to_owned();
            return;
        }
        let child_key = path_key(&child_file);
        let existing_child_source = if let Some(staged) =
            self.edit_session.staged_source(&child_key)
        {
            Some(staged.to_owned())
        } else if child_file.is_file() {
            let child_source = match read_consistent(&child_file) {
                Ok(source) => source,
                Err(error) => {
                    self.sheet_edit = Some(edit);
                    self.status = format!("Could not inspect child sheet: {error}");
                    return;
                }
            };
            Some(child_source)
        } else if child_file.exists() {
            self.sheet_edit = Some(edit);
            self.status = "The child schematic path exists but is not a regular file".to_owned();
            return;
        } else {
            None
        };
        let (parent_instance_path, page) = match next_sheet_instance(&source) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.sheet_edit = Some(edit);
                self.status = format!("Sheet metadata rejected: {error:#}");
                return;
            }
        };
        let project_name = self
            .root
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("project")
            .to_owned();
        let block = konnect_sexp::schematic::format_hierarchical_sheet(HierarchicalSheetSpec {
            name: &name,
            file: &file_name,
            x: edit.point.x,
            y: edit.point.y,
            width: 80.0,
            height: 50.0,
            project_name: &project_name,
            parent_instance_path: &parent_instance_path,
            page: &page,
        });
        let command = match SchematicCommand::insert_item(
            &source,
            block,
            ItemAnchor::BeforeFooter,
            "Add hierarchical sheet",
        ) {
            Ok(command) => command.requiring_unchanged_document(),
            Err(error) => {
                self.sheet_edit = Some(edit);
                self.status = format!("Sheet creation rejected: {error}");
                return;
            }
        };
        let Some(sheet_uuid) = command.changes.first().map(|change| change.id.to_string()) else {
            self.sheet_edit = Some(edit);
            self.status = "Sheet insertion unexpectedly produced no item change".to_owned();
            return;
        };
        let hierarchy_path = format!(
            "{}/{}",
            parent_instance_path.trim_end_matches('/'),
            sheet_uuid
        );
        let child_patch = match &existing_child_source {
            Some(child_source) => match SchematicCommand::ensure_symbol_instance_path(
                child_source,
                &project_name,
                &hierarchy_path,
                "Link hierarchical child symbols",
            ) {
                Ok(Some(child_command)) => {
                    match prepare_command(&child_file, child_source, &child_command) {
                        Ok((replacement, outcome)) => {
                            Some((replacement, outcome, child_command.changes.len()))
                        }
                        Err(error) => {
                            self.sheet_edit = Some(edit);
                            self.status = format!("Child instance patch conflict: {error}");
                            return;
                        }
                    }
                }
                Ok(None) => None,
                Err(error) => {
                    self.sheet_edit = Some(edit);
                    self.status = format!("Child instance patch rejected: {error}");
                    return;
                }
            },
            None => None,
        };
        let (parent_after, parent_outcome) = match prepare_command(&parent_file, &source, &command)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                self.sheet_edit = Some(edit);
                self.status = format!("Sheet creation conflict: {error}");
                return;
            }
        };
        let child_creation = existing_child_source
            .is_none()
            .then(konnect_sexp::schematic::format_blank_schematic);
        let mut staged_session = self.edit_session.clone();
        let staging_result = staged_session
            .stage_replacement(
                path_key(&parent_file),
                &parent_file,
                &source,
                parent_after.clone(),
            )
            .and_then(|()| {
                if let Some(child_source) = &child_creation {
                    staged_session.stage_creation(
                        child_key.clone(),
                        &child_file,
                        child_source.clone(),
                    )?;
                }
                if let (Some(child_source), Some((replacement, _, _))) =
                    (&existing_child_source, &child_patch)
                {
                    staged_session.stage_replacement(
                        child_key.clone(),
                        &child_file,
                        child_source,
                        replacement.clone(),
                    )?;
                }
                Ok(())
            });
        match staging_result {
            Ok(()) => {
                self.edit_session = staged_session;
                self.apply_staged_source(&parent_file, parent_after);
                self.record_command_change(ChangeOrigin::Local, &parent_file, &command);
                let history = if let Some((_, child_outcome, _)) = &child_patch {
                    HistoryEntry::group(
                        parent_dir.clone(),
                        vec![
                            HistoryCommand {
                                file: parent_file.clone(),
                                command: parent_outcome.inverse,
                            },
                            HistoryCommand {
                                file: child_file.clone(),
                                command: child_outcome.inverse.clone(),
                            },
                        ],
                    )
                } else {
                    HistoryEntry::single(parent_file.clone(), parent_outcome.inverse)
                };
                push_history(&mut self.undo_stack, history);
                self.redo_stack.clear();
                self.status = if let Some((_, _, count)) = child_patch {
                    format!(
                        "Staged {file_name} link and {count} child symbol patch(es) · press Commit"
                    )
                } else if child_creation.is_some() {
                    format!("Staged new child {file_name} and link · press Commit")
                } else {
                    format!("Staged existing child link {file_name} · press Commit")
                };
            }
            Err(error) => {
                self.sheet_edit = Some(edit);
                self.status = format!("Sheet staging stopped safely: {error}");
            }
        }
    }

    fn undo(&mut self) {
        self.apply_history(true);
    }

    fn redo(&mut self) {
        self.apply_history(false);
    }

    fn apply_history(&mut self, undoing: bool) {
        if !self.require_edit_mode() {
            return;
        }
        let entry = if undoing {
            self.undo_stack.pop()
        } else {
            self.redo_stack.pop()
        };
        let Some(entry) = entry else {
            self.status = if undoing {
                "Nothing to undo".to_owned()
            } else {
                "Nothing to redo".to_owned()
            };
            return;
        };

        if entry.journal_root.is_some() || entry.commands.len() > 1 {
            self.apply_grouped_history(undoing, entry);
            return;
        }
        let Some(part) = entry.commands.into_iter().next() else {
            self.status = "History entry had no commands".to_owned();
            return;
        };
        match self.stage_command(&part.file, &part.command) {
            Ok(outcome) => {
                self.record_command_change(
                    if undoing {
                        ChangeOrigin::Undo
                    } else {
                        ChangeOrigin::Redo
                    },
                    &part.file,
                    &part.command,
                );
                let inverse = HistoryEntry::single(part.file.clone(), outcome.inverse);
                if undoing {
                    push_history(&mut self.redo_stack, inverse);
                } else {
                    push_history(&mut self.undo_stack, inverse);
                }
                let rebased = if outcome.rebased {
                    " · safely rebased"
                } else {
                    ""
                };
                self.status = format!("Staged {}{rebased} · press Commit", part.command.label);
            }
            Err(error @ (SexpError::Conflict { .. } | SexpError::ItemConflict { .. })) => {
                self.restore_history_entry(undoing, HistoryEntry::single(part.file, part.command));
                self.status = format!("Staged history conflict: {error}");
            }
            Err(error) => {
                self.restore_history_entry(undoing, HistoryEntry::single(part.file, part.command));
                self.status = format!("Could not stage history operation: {error}");
            }
        }
    }

    fn apply_grouped_history(&mut self, undoing: bool, entry: HistoryEntry) {
        let Some(journal_root) = entry.journal_root.clone() else {
            self.restore_history_entry(undoing, entry);
            self.status = "Grouped history has no project transaction directory".to_owned();
            return;
        };
        let label = entry
            .commands
            .first()
            .map(|part| part.command.label.clone())
            .unwrap_or_else(|| "grouped edit".to_owned());
        let mut candidate = self.edit_session.clone();
        let mut inverses = Vec::with_capacity(entry.commands.len());
        let mut rendered_sources = Vec::with_capacity(entry.commands.len());
        for part in &entry.commands {
            let key = path_key(&part.file);
            let current = candidate
                .staged_source(&key)
                .map(str::to_owned)
                .or_else(|| {
                    self.sheets
                        .iter()
                        .find(|sheet| path_key(&sheet.file) == key)
                        .map(|sheet| sheet.semantic.source.to_string())
                })
                .or_else(|| read_consistent(&part.file).ok());
            let Some(current) = current else {
                self.restore_history_entry(undoing, entry.clone());
                self.status = format!("Could not read {} for staged history", part.file.display());
                return;
            };
            let (replacement, outcome) = match prepare_command(&part.file, &current, &part.command)
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.restore_history_entry(undoing, entry.clone());
                    self.status = format!("Grouped staging conflict: {error}");
                    return;
                }
            };
            if let Err(error) =
                candidate.stage_replacement(key, &part.file, &current, replacement.clone())
            {
                self.restore_history_entry(undoing, entry.clone());
                self.status = format!("Grouped staging conflict: {error}");
                return;
            }
            inverses.push(HistoryCommand {
                file: part.file.clone(),
                command: outcome.inverse,
            });
            rendered_sources.push((part.file.clone(), replacement));
        }
        self.edit_session = candidate;
        for (file, source) in rendered_sources {
            self.apply_staged_source(&file, source);
        }
        let inverse = HistoryEntry::group(journal_root.clone(), inverses);
        let timeline_uuids = inverse
            .commands
            .iter()
            .flat_map(|part| part.command.changes.iter())
            .map(|change| change.id.as_str().to_owned())
            .collect();
        self.record_change(
            if undoing {
                ChangeOrigin::Undo
            } else {
                ChangeOrigin::Redo
            },
            label.clone(),
            journal_root,
            timeline_uuids,
        );
        if undoing {
            push_history(&mut self.redo_stack, inverse);
        } else {
            push_history(&mut self.undo_stack, inverse);
        }
        self.status = format!("Staged {label} across multiple files · press Commit");
    }

    fn restore_history_entry(&mut self, undoing: bool, entry: HistoryEntry) {
        if undoing {
            push_history(&mut self.undo_stack, entry);
        } else {
            push_history(&mut self.redo_stack, entry);
        }
    }

    fn reconcile_watch_dirs(&mut self) {
        let wanted = self
            .sheets
            .iter()
            .filter_map(|sheet| sheet.file.parent().map(Path::to_path_buf))
            .collect::<HashSet<_>>();
        for directory in wanted.difference(&self.watched_dirs) {
            if let Err(error) = self.watcher.watch(directory, RecursiveMode::NonRecursive) {
                self.status = format!("Watch error: {error}");
            }
        }
        for directory in self.watched_dirs.difference(&wanted) {
            let _ = self.watcher.unwatch(directory);
        }
        self.watched_dirs = wanted;
    }

    fn schedule_reload(&mut self, changed: &[PathBuf]) {
        self.schedule_reload_with_origin(changed, false);
    }

    fn schedule_external_reload(&mut self, changed: &[PathBuf]) {
        self.schedule_reload_with_origin(changed, true);
    }

    fn schedule_reload_with_origin(&mut self, changed: &[PathBuf], external: bool) {
        let generation = self.reload_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let request = ReloadRequest {
            generation,
            root: self.root.clone(),
            changed: changed
                .iter()
                .map(|path| path_key(path))
                .collect::<HashSet<_>>(),
            known: self
                .sheets
                .iter()
                .map(|sheet| path_key(&sheet.file))
                .collect(),
            external,
        };
        if self.reload_tx.send(request).is_err() {
            self.status = "Background reload worker stopped unexpectedly".to_owned();
        }
    }

    fn apply_reload_batch(&mut self, batch: ReloadBatch) {
        if batch.generation != self.reload_generation.load(Ordering::Acquire) {
            return;
        }
        let external = batch.external;
        let entries = match batch.entries {
            Ok(entries) => entries,
            Err(error) => {
                self.status = format!("Hierarchy reload failed: {error}");
                return;
            }
        };
        let active_key = self
            .sheets
            .get(self.active)
            .map(|sheet| path_key(&sheet.file));
        let selected = std::mem::take(&mut self.selected_uuids);
        let palette = self.palette();
        let mut old = self
            .sheets
            .drain(..)
            .map(|sheet| (path_key(&sheet.file), sheet))
            .collect::<HashMap<_, _>>();
        let mut loaded = batch.loaded;
        let mut next = Vec::with_capacity(entries.len());
        let mut errors = Vec::new();
        let mut external_lines = Vec::new();
        let mut external_changes = Vec::new();

        for entry in entries {
            let key = path_key(&entry.file);
            match loaded.remove(&key) {
                Some(Ok(loaded_scene)) => {
                    if external {
                        if let Some(previous) = old.get(&key) {
                            if let Some(summary) = summarize_external_change(
                                &entry.name,
                                &previous.semantic.source,
                                &loaded_scene.semantic.source,
                            ) {
                                external_lines.push(summary.description.clone());
                                external_changes.push((
                                    summary.description,
                                    entry.file.clone(),
                                    summary.changed_uuids,
                                ));
                            }
                        }
                    }
                    let rendered = encode_scene(&loaded_scene.semantic, palette);
                    next.push(NativeSheet {
                        name: entry.name,
                        depth: entry.depth,
                        file: entry.file,
                        semantic: loaded_scene.semantic,
                        rendered,
                        compatibility: loaded_scene.compatibility,
                        compatibility_error: loaded_scene.compatibility_error,
                    });
                }
                Some(Err(error)) => {
                    errors.push(format!("{}: {error}", entry.name));
                    if let Some(mut sheet) = old.remove(&key) {
                        sheet.name = entry.name;
                        sheet.depth = entry.depth;
                        sheet.file = entry.file;
                        next.push(sheet);
                    }
                }
                None => {
                    if let Some(mut sheet) = old.remove(&key) {
                        sheet.name = entry.name;
                        sheet.depth = entry.depth;
                        sheet.file = entry.file;
                        next.push(sheet);
                    } else {
                        errors.push(format!(
                            "{}: background reload returned no scene",
                            entry.name
                        ));
                    }
                }
            }
        }

        self.sheets = next;
        self.active = active_key
            .and_then(|key| {
                self.sheets
                    .iter()
                    .position(|sheet| path_key(&sheet.file) == key)
            })
            .unwrap_or(0)
            .min(self.sheets.len().saturating_sub(1));
        self.selected_uuids = selected
            .into_iter()
            .filter(|uuid| {
                self.sheets.get(self.active).is_some_and(|sheet| {
                    sheet
                        .semantic
                        .objects
                        .iter()
                        .any(|object| &object.uuid == uuid)
                })
            })
            .collect();
        self.status = if errors.is_empty() {
            self.live_status()
        } else {
            format!("Render error: {}", errors.join("; "))
        };
        if external && !external_lines.is_empty() {
            self.external_preview = Some(ExternalChangePreview {
                lines: external_lines,
            });
            if errors.is_empty() {
                self.status =
                    "External schematic update loaded · review summary · Esc dismisses".to_owned();
            }
        }
        let mut latest_external = None;
        for (label, file, uuids) in external_changes {
            latest_external = Some(self.record_change(ChangeOrigin::External, label, file, uuids));
        }
        if self.settings.follow_changes {
            self.pending_follow = latest_external;
        }
        if self.edit_session.is_conflicted() {
            self.status = format!(
                "Commit blocked · {} staged file(s) changed externally; Discard to load them",
                self.edit_session.conflicted_count()
            );
        }
        self.reconcile_watch_dirs();
    }

    fn start_text_selection(&mut self, cursor: PhysicalPosition<f64>) -> bool {
        let Some(target) = self
            .text_targets
            .iter()
            .rev()
            .find(|target| target.rect.contains(cursor.x, cursor.y))
            .cloned()
        else {
            self.text_drag = None;
            self.text_selection = None;
            self.status = "Text selection · drag over text to select".to_owned();
            return false;
        };
        let anchor = if target.select_whole {
            0
        } else {
            target.character_at(cursor.x)
        };
        let current = if target.select_whole {
            target.character_count()
        } else {
            anchor
        };
        self.text_drag = Some(TextDrag {
            target: target.clone(),
            anchor,
            current,
        });
        self.text_selection = Some(TextSelection {
            target,
            start: anchor,
            end: current,
        });
        true
    }

    fn update_text_selection(&mut self, cursor: PhysicalPosition<f64>) {
        let Some(drag) = &mut self.text_drag else {
            return;
        };
        drag.current = if drag.target.select_whole {
            drag.target.character_count()
        } else {
            drag.target.character_at(cursor.x)
        };
        self.text_selection = Some(TextSelection {
            target: drag.target.clone(),
            start: drag.anchor,
            end: drag.current,
        });
    }

    fn finish_text_selection(&mut self) {
        let Some(drag) = self.text_drag.take() else {
            return;
        };
        let (start, end) = if drag.target.select_whole || drag.anchor == drag.current {
            (0, drag.target.character_count())
        } else {
            (drag.anchor, drag.current)
        };
        let selected = drag.target.selected_text(start, end);
        self.text_selection = Some(TextSelection {
            target: drag.target,
            start,
            end,
        });
        self.status = format!(
            "Selected {} character(s) · Ctrl/Command C to copy",
            selected.chars().count()
        );
    }

    fn copy_selected_text(&mut self) {
        let Some(selection) = &self.text_selection else {
            self.status = "Nothing selected · enable text selection and drag text first".to_owned();
            return;
        };
        let text = selection
            .target
            .selected_text(selection.start, selection.end);
        if text.is_empty() {
            self.status = "The text selection is empty".to_owned();
            return;
        }
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        match self
            .clipboard
            .as_mut()
            .ok_or_else(|| anyhow!("platform clipboard is unavailable"))
            .and_then(|clipboard| {
                clipboard
                    .set_text(text.clone())
                    .map_err(anyhow::Error::from)
            }) {
            Ok(()) => self.status = format!("Copied {} character(s)", text.chars().count()),
            Err(error) => self.status = format!("Could not copy text: {error:#}"),
        }
    }

    fn register_schematic_text(&mut self, transform: Affine) {
        if !self.text_select_mode {
            return;
        }
        let Some(sheet) = self.sheets.get(self.active) else {
            return;
        };
        self.text_targets
            .extend(sheet.semantic.primitives.iter().filter_map(|primitive| {
                let Primitive::Text { text, .. } = primitive else {
                    return None;
                };
                if text.is_empty() {
                    return None;
                }
                let bounds = primitive.bounds()?;
                let first = transform * KurboPoint::new(bounds.min_x, bounds.min_y);
                let second = transform * KurboPoint::new(bounds.max_x, bounds.max_y);
                Some(SelectableText::whole(
                    text,
                    ScreenRect {
                        x0: first.x.min(second.x) - 3.0,
                        y0: first.y.min(second.y) - 3.0,
                        x1: first.x.max(second.x) + 3.0,
                        y1: first.y.max(second.y) + 3.0,
                    },
                ))
            }));
    }

    fn draw_text_selection(&mut self, palette: Palette) {
        let Some(selection) = &mut self.text_selection else {
            return;
        };
        let previous_center = selection.target.rect.center();
        if let Some(current) = self
            .text_targets
            .iter()
            .filter(|target| target.text == selection.target.text)
            .min_by(|left, right| {
                let left_center = left.rect.center();
                let right_center = right.rect.center();
                let left_distance = (left_center.0 - previous_center.0).powi(2)
                    + (left_center.1 - previous_center.1).powi(2);
                let right_distance = (right_center.0 - previous_center.0).powi(2)
                    + (right_center.1 - previous_center.1).powi(2);
                left_distance.total_cmp(&right_distance)
            })
        {
            selection.target.clone_from(current);
        }
        let rect = selection
            .target
            .selection_rect(selection.start, selection.end);
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.accent.with_alpha(0.3),
            None,
            &RoundedRect::new(rect.x0, rect.y0, rect.x1, rect.y1, 2.0),
        );
        self.frame.stroke(
            &Stroke::new(1.0),
            Affine::IDENTITY,
            palette.accent,
            None,
            &RoundedRect::new(rect.x0, rect.y0, rect.x1, rect.y1, 2.0),
        );
    }

    fn draw_frame(&mut self, width: u32, height: u32) {
        let width = f64::from(width);
        let height = f64::from(height);
        self.apply_pending_follow(width, height);
        let palette = self.palette();
        self.frame.reset();
        self.text_targets.clear();
        let drag_delta = self
            .item_drag
            .as_ref()
            .and_then(|drag| self.drag_delta(width, height, drag));
        let highlighted = (self.item_drag.is_none() && self.settings.highlight_changes)
            .then_some(self.highlighted_change)
            .flatten()
            .and_then(|id| self.timeline.event(id))
            .map(|event| (path_key(&event.file), event.origin, event.uuids.clone()));

        if let Some(sheet) = self.sheets.get(self.active) {
            if let Some((transform, scale, _, _)) = self.page_transform(width, height) {
                if let Some(drag) = &self.item_drag {
                    self.frame.append(&drag.base_scene, Some(transform));
                } else {
                    append_sheet(&mut self.frame, sheet, transform);
                }
                if let Some((file, origin, uuids)) = &highlighted {
                    if path_key(&sheet.file) == *file {
                        let mut highlight_palette = palette;
                        highlight_palette.selection = match origin {
                            ChangeOrigin::External => palette.bus,
                            ChangeOrigin::Local => palette.accent,
                            ChangeOrigin::Undo => palette.selection,
                            ChangeOrigin::Redo => palette.junction,
                        };
                        for object in sheet
                            .semantic
                            .objects
                            .iter()
                            .filter(|object| uuids.contains(&object.uuid))
                        {
                            append_selection(
                                &mut self.frame,
                                sheet,
                                object,
                                transform,
                                scale,
                                highlight_palette,
                            );
                        }
                    }
                }
                for diagnostic in &sheet.semantic.diagnostics {
                    let radius = (6.0 / scale).clamp(0.45, 1.75);
                    let color = match diagnostic.kind {
                        ConnectivityDiagnosticKind::ConnectedNoConnect => palette.no_connect,
                        ConnectivityDiagnosticKind::DanglingWire => palette.selection,
                        ConnectivityDiagnosticKind::DuplicateReference => palette.accent,
                        ConnectivityDiagnosticKind::DuplicateSheetName => palette.accent,
                        ConnectivityDiagnosticKind::DuplicateSheetPin => palette.accent,
                        ConnectivityDiagnosticKind::MissingJunction => palette.accent,
                        ConnectivityDiagnosticKind::UnconnectedBusEntry => palette.bus,
                        ConnectivityDiagnosticKind::UnpositionedSheetField => palette.sheet_file,
                    };
                    self.frame.stroke(
                        &Stroke::new((2.0 / scale).max(0.1)),
                        transform,
                        color,
                        None,
                        &Circle::new((diagnostic.point.x, diagnostic.point.y), radius),
                    );
                    if diagnostic.kind == ConnectivityDiagnosticKind::MissingJunction {
                        let diagonal = radius * 0.65;
                        for (dx0, dy0, dx1, dy1) in [
                            (-diagonal, -diagonal, diagonal, diagonal),
                            (-diagonal, diagonal, diagonal, -diagonal),
                        ] {
                            self.frame.stroke(
                                &Stroke::new((1.5 / scale).max(0.08)),
                                transform,
                                color,
                                None,
                                &Line::new(
                                    (diagnostic.point.x + dx0, diagnostic.point.y + dy0),
                                    (diagnostic.point.x + dx1, diagnostic.point.y + dy1),
                                ),
                            );
                        }
                    }
                }
                if let Some(wire) = self.wire_draft {
                    let line = Line::new(
                        (wire.start.x, wire.start.y),
                        (wire.current.x, wire.current.y),
                    );
                    let color = if wire.is_bus {
                        palette.bus
                    } else {
                        palette.wire
                    };
                    self.frame
                        .stroke(&round_stroke(0.254), transform, color, None, &line);
                    let endpoint_radius = (4.0 / scale).clamp(0.35, 1.25);
                    for point in [wire.start, wire.current] {
                        self.frame.stroke(
                            &Stroke::new((1.5 / scale).max(0.08)),
                            transform,
                            palette.selection,
                            None,
                            &Circle::new((point.x, point.y), endpoint_radius),
                        );
                    }
                }
                if let (Some(drag), Some((dx, dy))) = (&self.item_drag, drag_delta) {
                    for wire in &drag.connected_wires {
                        let start = if wire.move_start {
                            (wire.start.x + dx, wire.start.y + dy)
                        } else {
                            (wire.start.x, wire.start.y)
                        };
                        let end = if wire.move_end {
                            (wire.end.x + dx, wire.end.y + dy)
                        } else {
                            (wire.end.x, wire.end.y)
                        };
                        self.frame.stroke(
                            &round_stroke(0.254),
                            transform,
                            palette.wire,
                            None,
                            &Line::new(start, end),
                        );
                    }
                }
                let selection_transform = drag_delta.map_or(transform, |(dx, dy)| {
                    transform * Affine::translate((dx, dy))
                });
                for uuid in &self.selected_uuids {
                    if let Some(object) = sheet
                        .semantic
                        .objects
                        .iter()
                        .find(|object| &object.uuid == uuid)
                    {
                        if drag_delta.is_some() {
                            append_object_artwork(
                                &mut self.frame,
                                sheet,
                                object,
                                selection_transform,
                                palette,
                            );
                        }
                        append_selection(
                            &mut self.frame,
                            sheet,
                            object,
                            selection_transform,
                            scale,
                            palette,
                        );
                    }
                }
            }
        }

        if let Some((transform, _, _, _)) = self.page_transform(width, height) {
            self.register_schematic_text(transform);
        }

        if let Some(selection) = self.selection_box {
            let x0 = selection.start.x.min(selection.current.x);
            let x1 = selection.start.x.max(selection.current.x);
            let y0 = selection.start.y.min(selection.current.y);
            let y1 = selection.start.y.max(selection.current.y);
            let rect = Rect::new(x0, y0, x1, y1);
            self.frame.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                palette.selection.with_alpha(0.12),
                None,
                &rect,
            );
            self.frame.stroke(
                &Stroke::new(1.25),
                Affine::IDENTITY,
                palette.selection,
                None,
                &rect,
            );
        }

        self.draw_toolbar(width, height, palette);
        self.draw_filmstrip(width, height, palette);
        self.draw_external_preview(width, palette);
        self.draw_diagnostics(width, height, palette);
        self.draw_inspector(width, height, palette);
        self.draw_search(width, palette);
        self.draw_property_edit(width, palette);
        self.draw_label_edit(width, palette);
        self.draw_sheet_edit(width, palette);
        self.draw_sheet_pin_edit(width, palette);
        self.draw_text_selection(palette);
    }

    fn draw_sheet_pin_edit(&mut self, width: f64, palette: Palette) {
        let Some(edit) = &self.sheet_pin_edit else {
            return;
        };
        let x0 = (width / 2.0 - 270.0).max(12.0);
        let x1 = (width / 2.0 + 270.0).min(width - 12.0);
        let y0 = STATUS_HEIGHT + 90.0;
        let rect = RoundedRect::new(x0, y0, x1, y0 + 104.0, 7.0);
        self.frame
            .fill(Fill::NonZero, Affine::IDENTITY, palette.card, None, &rect);
        self.frame.stroke(
            &Stroke::new(1.5),
            Affine::IDENTITY,
            palette.accent,
            None,
            &rect,
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &format!(
                "Sheet pin at {:.3}, {:.3} mm · type {}",
                edit.point.x,
                edit.point.y,
                edit.pin_type.keyword()
            ),
            TextRun {
                size: 13.0,
                position: (x0 + 14.0, y0 + 23.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.accent,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &format!("Name: {}▏", truncate_ui(&edit.name, 54)),
            TextRun {
                size: 14.0,
                position: (x0 + 14.0, y0 + 52.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            "Tab · cycle type    Enter · place    Esc · cancel",
            TextRun {
                size: 10.0,
                position: (x0 + 14.0, y0 + 86.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
    }

    fn draw_sheet_edit(&mut self, width: f64, palette: Palette) {
        let Some(edit) = &self.sheet_edit else {
            return;
        };
        let x0 = (width / 2.0 - 280.0).max(12.0);
        let x1 = (width / 2.0 + 280.0).min(width - 12.0);
        let y0 = STATUS_HEIGHT + 82.0;
        let rect = RoundedRect::new(x0, y0, x1, y0 + 132.0, 7.0);
        self.frame
            .fill(Fill::NonZero, Affine::IDENTITY, palette.card, None, &rect);
        self.frame.stroke(
            &Stroke::new(1.5),
            Affine::IDENTITY,
            palette.accent,
            None,
            &rect,
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &format!(
                "Hierarchical sheet at {:.3}, {:.3} mm",
                edit.point.x, edit.point.y
            ),
            TextRun {
                size: 13.0,
                position: (x0 + 14.0, y0 + 21.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.accent,
            },
        );
        for (index, (field, value)) in [
            (SheetEditField::Name, edit.name.as_str()),
            (SheetEditField::File, edit.file.as_str()),
        ]
        .into_iter()
        .enumerate()
        {
            let active = edit.field == field;
            let label = match field {
                SheetEditField::Name => "Name",
                SheetEditField::File => "Child file",
            };
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &format!(
                    "{label}: {}{}",
                    truncate_ui(value, 54),
                    if active { "▏" } else { "" }
                ),
                TextRun {
                    size: 14.0,
                    position: (x0 + 14.0, y0 + 50.0 + index as f64 * 29.0),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: if active { palette.accent } else { palette.text },
                },
            );
        }
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            "Tab · switch field    Enter · create/link    Esc · cancel",
            TextRun {
                size: 10.0,
                position: (x0 + 14.0, y0 + 116.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
    }

    fn draw_label_edit(&mut self, width: f64, palette: Palette) {
        let Some(edit) = &self.label_edit else {
            return;
        };
        let x0 = (width / 2.0 - 250.0).max(12.0);
        let x1 = (width / 2.0 + 250.0).min(width - 12.0);
        let y0 = STATUS_HEIGHT + 90.0;
        let rect = RoundedRect::new(x0, y0, x1, y0 + 82.0, 7.0);
        self.frame
            .fill(Fill::NonZero, Affine::IDENTITY, palette.card, None, &rect);
        self.frame.stroke(
            &Stroke::new(1.5),
            Affine::IDENTITY,
            palette.accent,
            None,
            &rect,
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &format!("Local label at {:.3}, {:.3} mm", edit.point.x, edit.point.y),
            TextRun {
                size: 12.0,
                position: (x0 + 14.0, y0 + 20.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.accent,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &truncate_ui(&format!("{}▏", edit.value), 58),
            TextRun {
                size: 16.0,
                position: (x0 + 14.0, y0 + 48.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            "Enter · atomic place    Esc · cancel",
            TextRun {
                size: 10.0,
                position: (x0 + 14.0, y0 + 68.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
    }

    fn draw_external_preview(&mut self, width: f64, palette: Palette) {
        let Some(preview) = &self.external_preview else {
            return;
        };
        let visible = preview.lines.len().min(6);
        let x0 = (width - 560.0).max(SIDEBAR_WIDTH + 14.0);
        let y0 = STATUS_HEIGHT + 8.0;
        let x1 = (x0 + 546.0).min(width - 14.0);
        let y1 = y0 + 58.0 + visible as f64 * 31.0;
        let panel = RoundedRect::new(x0, y0, x1, y1, 7.0);
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.card.with_alpha(0.97),
            None,
            &panel,
        );
        self.frame.stroke(
            &Stroke::new(1.5),
            Affine::IDENTITY,
            palette.accent,
            None,
            &panel,
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            "External schematic update · loaded safely",
            TextRun {
                size: 15.0,
                position: (x0 + 14.0, y0 + 23.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.accent,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            "Item preconditions remain active · Esc dismisses",
            TextRun {
                size: 11.0,
                position: (x0 + 14.0, y0 + 44.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
        for (index, line) in preview.lines.iter().take(visible).enumerate() {
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &truncate_ui(line, 72),
                TextRun {
                    size: 12.0,
                    position: (x0 + 14.0, y0 + 70.0 + index as f64 * 31.0),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: palette.text,
                },
            );
        }
    }

    fn draw_property_edit(&mut self, width: f64, palette: Palette) {
        let Some(edit) = &self.property_edit else {
            return;
        };
        let x0 = (width / 2.0 - 250.0).max(12.0);
        let x1 = (width / 2.0 + 250.0).min(width - 12.0);
        let y0 = STATUS_HEIGHT + 90.0;
        let rect = RoundedRect::new(x0, y0, x1, y0 + 82.0, 7.0);
        self.frame
            .fill(Fill::NonZero, Affine::IDENTITY, palette.card, None, &rect);
        self.frame.stroke(
            &Stroke::new(1.5),
            Affine::IDENTITY,
            palette.selection,
            None,
            &rect,
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &format!("Edit {}", edit.name),
            TextRun {
                size: 13.0,
                position: (x0 + 14.0, y0 + 20.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.accent,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &truncate_ui(&format!("{}▏", edit.value), 58),
            TextRun {
                size: 16.0,
                position: (x0 + 14.0, y0 + 48.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            "Enter · atomic save    Esc · cancel",
            TextRun {
                size: 10.0,
                position: (x0 + 14.0, y0 + 68.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
    }

    fn draw_inspector(&mut self, width: f64, height: f64, palette: Palette) {
        if self.selected_uuids.len() != 1 {
            return;
        }
        let Some(uuid) = self.selected_uuids.iter().next() else {
            return;
        };
        let Some(object) = self.sheets.get(self.active).and_then(|sheet| {
            sheet
                .semantic
                .objects
                .iter()
                .find(|object| &object.uuid == uuid)
        }) else {
            return;
        };
        let x0 = (width - 310.0).max(0.0);
        let y0 = STATUS_HEIGHT + 8.0;
        let y1 = (height - FILMSTRIP_HEIGHT - 8.0).max(y0 + 80.0);
        let panel = RoundedRect::new(x0, y0, width - 8.0, y1, 7.0);
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.card.with_alpha(0.96),
            None,
            &panel,
        );
        self.frame.stroke(
            &Stroke::new(1.0),
            Affine::IDENTITY,
            palette.card_border,
            None,
            &panel,
        );
        let mut lines = vec![
            ("Properties".to_owned(), 15.0_f32, palette.accent),
            (
                truncate_ui(&format!("{:?} · {}", object.kind, object.label), 42),
                12.0,
                palette.text,
            ),
            (
                truncate_ui(&format!("UUID · {}", object.uuid), 42),
                10.0,
                palette.text,
            ),
            ("E · edit primary property".to_owned(), 10.0, palette.accent),
        ];
        lines.extend(object.properties.iter().map(|property| {
            (
                truncate_ui(&format!("{} · {}", property.name, property.value), 44),
                11.0,
                palette.text,
            )
        }));
        let mut line_y = y0 + 21.0;
        for (text, size, color) in lines {
            if line_y > y1 - 14.0 {
                draw_selectable_text(
                    &self.font,
                    &mut self.frame,
                    &mut self.text_targets,
                    "…",
                    TextRun {
                        size: 12.0,
                        position: (x0 + 12.0, line_y),
                        rotation_deg: 0.0,
                        align: TextAlign::Left,
                        color: palette.text,
                    },
                );
                break;
            }
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &text,
                TextRun {
                    size,
                    position: (x0 + 12.0, line_y),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color,
                },
            );
            line_y += f64::from(size) + 7.0;
        }
    }

    fn draw_diagnostics(&mut self, width: f64, height: f64, palette: Palette) {
        let diagnostic_count = self
            .sheets
            .get(self.active)
            .map_or(0, |sheet| sheet.semantic.diagnostics.len());
        if diagnostic_count == 0 {
            return;
        }
        let visible = diagnostic_count.min(4);
        let panel_rect = self.diagnostics_panel_rect(width, height, diagnostic_count);
        let ScreenRect { x0, y0, x1, y1 } = panel_rect;
        let line_height = 36.0 * f64::from(self.settings.ui_scale).min(1.35);
        let panel = RoundedRect::new(x0, y0, x1, y1, 9.0);
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.card.with_alpha(0.96),
            None,
            &panel,
        );
        self.frame.stroke(
            &Stroke::new(1.0),
            Affine::IDENTITY,
            palette.selection,
            None,
            &panel,
        );
        for row in 0..3 {
            for column in 0..2 {
                self.frame.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    palette.selection,
                    None,
                    &Circle::new(
                        (
                            x0 + 13.0 + column as f64 * 5.0,
                            y0 + 14.0 + row as f64 * 5.0,
                        ),
                        1.2,
                    ),
                );
            }
        }
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &if self.settings.diagnostics_collapsed {
                format!("{diagnostic_count} design warnings")
            } else {
                format!("Design checks · {diagnostic_count} warning(s)")
            },
            TextRun {
                size: if self.settings.diagnostics_collapsed {
                    12.0
                } else {
                    16.0
                },
                position: (x0 + 31.0, y0 + DIAGNOSTICS_HEADER_HEIGHT / 2.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.selection,
            },
        );
        let collapse_rect = Self::diagnostics_collapse_rect(panel_rect);
        let collapse_button = RoundedRect::new(
            collapse_rect.x0,
            collapse_rect.y0,
            collapse_rect.x1,
            collapse_rect.y1,
            5.0,
        );
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.fill.with_alpha(0.42),
            None,
            &collapse_button,
        );
        self.frame.stroke(
            &Stroke::new(1.0),
            Affine::IDENTITY,
            palette.card_border,
            None,
            &collapse_button,
        );
        let (collapse_x, collapse_y) = collapse_rect.center();
        self.frame.stroke(
            &Stroke::new(1.8),
            Affine::IDENTITY,
            palette.text,
            None,
            &Line::new(
                (collapse_x - 5.0, collapse_y),
                (collapse_x + 5.0, collapse_y),
            ),
        );
        if self.settings.diagnostics_collapsed {
            self.frame.stroke(
                &Stroke::new(1.8),
                Affine::IDENTITY,
                palette.text,
                None,
                &Line::new(
                    (collapse_x, collapse_y - 5.0),
                    (collapse_x, collapse_y + 5.0),
                ),
            );
            return;
        }
        let diagnostics = self.sheets[self.active]
            .semantic
            .diagnostics
            .iter()
            .take(visible)
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();
        for (index, diagnostic) in diagnostics.iter().enumerate() {
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &truncate_ui(diagnostic, 72),
                TextRun {
                    size: 13.0,
                    position: (
                        x0 + 16.0,
                        y0 + DIAGNOSTICS_HEADER_HEIGHT + 19.0 + index as f64 * line_height,
                    ),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: palette.text,
                },
            );
        }
        if diagnostic_count > visible {
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &format!("+{} more checks", diagnostic_count - visible),
                TextRun {
                    size: 11.0,
                    position: (x1 - 152.0, y0 + 24.0),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: palette.text,
                },
            );
        }
    }

    fn draw_search(&mut self, width: f64, palette: Palette) {
        let Some(search) = &self.search else {
            return;
        };
        let x0 = (width / 2.0 - 260.0).max(12.0);
        let y0 = STATUS_HEIGHT + 12.0;
        let rect = RoundedRect::new(
            x0,
            y0,
            (width / 2.0 + 260.0).min(width - 12.0),
            STATUS_HEIGHT + 72.0,
            7.0,
        );
        self.frame
            .fill(Fill::NonZero, Affine::IDENTITY, palette.card, None, &rect);
        self.frame.stroke(
            &Stroke::new(1.5),
            Affine::IDENTITY,
            palette.accent,
            None,
            &rect,
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &format!("Find: {}▏", search.query),
            TextRun {
                size: 15.0,
                position: (x0 + 14.0, y0 + 20.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
        let detail = search
            .hits
            .get(search.current.min(search.hits.len().saturating_sub(1)))
            .map_or_else(
                || {
                    format!(
                        "{} result(s) · Enter to navigate · Esc to close",
                        search.hits.len()
                    )
                },
                |hit| {
                    format!(
                        "{} result(s) · next: {} · Enter to navigate",
                        search.hits.len(),
                        hit.description
                    )
                },
            );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &detail,
            TextRun {
                size: 11.0,
                position: (x0 + 14.0, y0 + 44.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
    }

    fn draw_toolbar(&mut self, width: f64, height: f64, palette: Palette) {
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.toolbar,
            None,
            &Rect::new(0.0, 0.0, SIDEBAR_WIDTH, height),
        );
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.app.with_alpha(0.94),
            None,
            &Rect::new(SIDEBAR_WIDTH, 0.0, width, STATUS_HEIGHT),
        );
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            "KONNECT",
            TextRun {
                size: 10.5,
                position: (SIDEBAR_WIDTH / 2.0, STATUS_HEIGHT / 2.0),
                rotation_deg: 0.0,
                align: TextAlign::Center,
                color: palette.accent,
            },
        );
        let filename = self
            .sheets
            .get(self.active)
            .and_then(|sheet| sheet.file.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("schematic");
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            filename,
            TextRun {
                size: 12.0,
                position: (SIDEBAR_WIDTH + 14.0, STATUS_HEIGHT / 2.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.accent,
            },
        );
        let status_x = SIDEBAR_WIDTH + 24.0 + filename.chars().count() as f64 * 7.8;
        let controls_x = Self::edit_controls(width)[0].1.x0;
        let status_chars = ((controls_x - status_x - 12.0) / 6.3).max(0.0) as usize;
        let status = truncate_ui(&self.status, status_chars);
        draw_selectable_text(
            &self.font,
            &mut self.frame,
            &mut self.text_targets,
            &status,
            TextRun {
                size: 10.5,
                position: (status_x, STATUS_HEIGHT / 2.0),
                rotation_deg: 0.0,
                align: TextAlign::Left,
                color: palette.text,
            },
        );
        self.draw_edit_controls(width, palette);

        let hovered = self.cursor.and_then(|cursor| {
            Self::toolbar_buttons(width)
                .into_iter()
                .find(|(_, rect)| rect.contains(cursor.x, cursor.y))
                .map(|(action, _)| action)
        });
        for (action, rect) in Self::toolbar_buttons(width) {
            let active = self.toolbar_action_is_active(action);
            let is_hovered = hovered == Some(action);
            self.frame.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                if is_hovered {
                    palette.accent.with_alpha(0.16)
                } else if active {
                    palette.accent.with_alpha(0.1)
                } else {
                    palette.card
                },
                None,
                &RoundedRect::new(rect.x0, rect.y0, rect.x1, rect.y1, 7.0),
            );
            self.frame.stroke(
                &Stroke::new(if active || is_hovered { 1.5 } else { 1.0 }),
                Affine::IDENTITY,
                if active || is_hovered {
                    palette.accent
                } else {
                    palette.card_border
                },
                None,
                &RoundedRect::new(rect.x0, rect.y0, rect.x1, rect.y1, 7.0),
            );
            draw_ui_icon(
                &mut self.frame,
                Self::toolbar_icon(action),
                ScreenRect {
                    x0: rect.x0 + 7.0,
                    y0: rect.y0 + 7.0,
                    x1: rect.x1 - 7.0,
                    y1: rect.y1 - 7.0,
                },
                if active || is_hovered {
                    palette.accent
                } else {
                    palette.text
                },
            );
        }
        if let Some(action) = hovered {
            let label = self.toolbar_label(action);
            let (_, text_width) = self
                .font
                .measure_and_glyphs(10.0 * self.font.ui_scale, &label);
            let tooltip = ScreenRect {
                x0: SIDEBAR_WIDTH + 8.0,
                y0: self
                    .cursor
                    .map_or(48.0, |cursor| (cursor.y - 17.0).max(42.0)),
                x1: (SIDEBAR_WIDTH + 34.0 + f64::from(text_width)).min(width - 8.0),
                y1: self
                    .cursor
                    .map_or(82.0, |cursor| (cursor.y + 17.0).max(76.0)),
            };
            self.frame.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                palette.card.with_alpha(0.98),
                None,
                &RoundedRect::new(tooltip.x0, tooltip.y0, tooltip.x1, tooltip.y1, 6.0),
            );
            self.frame.stroke(
                &Stroke::new(1.0),
                Affine::IDENTITY,
                palette.card_border,
                None,
                &RoundedRect::new(tooltip.x0, tooltip.y0, tooltip.x1, tooltip.y1, 6.0),
            );
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &label,
                TextRun {
                    size: 10.0,
                    position: (tooltip.x0 + 10.0, tooltip.center().1),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: palette.text,
                },
            );
        }
    }

    fn draw_filmstrip(&mut self, width: f64, height: f64, palette: Palette) {
        let film = self.filmstrip_rect(width, height);
        self.frame.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            palette.filmstrip,
            None,
            &film.as_kurbo(),
        );

        for (index, sheet) in self.sheets.iter().enumerate() {
            let card = self.thumbnail_rect(index, height);
            if card.x1 < 0.0 || card.x0 > width {
                continue;
            }
            self.frame.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                palette.card,
                None,
                &RoundedRect::new(card.x0, card.y0, card.x1, card.y1, 6.0),
            );
            self.frame.stroke(
                &Stroke::new(if index == self.active { 3.0 } else { 1.0 }),
                Affine::IDENTITY,
                if index == self.active {
                    palette.accent
                } else {
                    palette.card_border
                },
                None,
                &RoundedRect::new(card.x0, card.y0, card.x1, card.y1, 6.0),
            );

            let preview = ScreenRect {
                x0: card.x0 + 7.0,
                y0: card.y0 + 7.0,
                x1: card.x1 - 7.0,
                y1: card.y1 - 29.0,
            };
            let scale = (preview.width() / sheet.semantic.width_mm)
                .min(preview.height() / sheet.semantic.height_mm);
            let tx = preview.center().0 - sheet.semantic.width_mm * scale / 2.0;
            let ty = preview.center().1 - sheet.semantic.height_mm * scale / 2.0;
            append_sheet(
                &mut self.frame,
                sheet,
                Affine::new([scale, 0.0, 0.0, scale, tx, ty]),
            );

            let hierarchy = "› ".repeat(sheet.depth);
            let thumbnail_label = truncate_ui(
                &format!(
                    "{:02}  {hierarchy}{}",
                    index + 1,
                    display_sheet_name(&sheet.name)
                ),
                22,
            );
            draw_selectable_text(
                &self.font,
                &mut self.frame,
                &mut self.text_targets,
                &thumbnail_label,
                TextRun {
                    size: 10.5,
                    position: (card.x0 + 7.0, card.y1 - 14.0),
                    rotation_deg: 0.0,
                    align: TextAlign::Left,
                    color: palette.text,
                },
            );
            if !sheet.semantic.coverage.is_complete() {
                draw_selectable_text(
                    &self.font,
                    &mut self.frame,
                    &mut self.text_targets,
                    "!",
                    TextRun {
                        size: 16.0,
                        position: (card.x1 - 14.0, card.y0 + 15.0),
                        rotation_deg: 0.0,
                        align: TextAlign::Center,
                        color: palette.selection,
                    },
                );
            }
        }
        self.draw_timeline(width, height, palette);
    }
}

impl ApplicationHandler<UserEvent> for VelloViewer {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = match self.cached_window.take() {
            Some(window) => window,
            None => match event_loop.create_window(window_attributes()) {
                Ok(window) => Arc::new(window),
                Err(error) => {
                    eprintln!("failed to create Vello window: {error}");
                    event_loop.exit();
                    return;
                }
            },
        };
        let size = window.inner_size();
        let surface = match pollster::block_on(self.context.create_surface(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        )) {
            Ok(surface) => surface,
            Err(error) => {
                eprintln!("failed to create Vello GPU surface: {error}");
                event_loop.exit();
                return;
            }
        };
        self.renderers
            .resize_with(self.context.devices.len(), || None);
        let device_id = surface.dev_id;
        if self.renderers[device_id].is_none() {
            let options = RendererOptions {
                use_cpu: false,
                antialiasing_support: [AaConfig::Area].into_iter().collect(),
                num_init_threads: None,
                pipeline_cache: None,
            };
            match Renderer::new(&self.context.devices[device_id].device, options) {
                Ok(renderer) => self.renderers[device_id] = Some(renderer),
                Err(error) => {
                    eprintln!("failed to initialize Vello renderer: {error}");
                    event_loop.exit();
                    return;
                }
            }
        }
        window.set_title("Konnect — Schematic Studio");
        window.request_redraw();
        self.state = Some(RenderState {
            surface,
            window,
            valid_surface: size.width > 0 && size.height > 0,
        });
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.take() {
            self.cached_window = Some(state.window);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = &self.state else {
            return;
        };
        if state.window.id() != window_id {
            return;
        }
        let width = f64::from(state.surface.config.width);
        let height = f64::from(state.surface.config.height);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(state) = &mut self.state {
                    state.valid_surface = size.width > 0 && size.height > 0;
                    if state.valid_surface {
                        self.context
                            .resize_surface(&mut state.surface, size.width, size.height);
                        state.window.request_redraw();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(drag) = self.diagnostics_drag {
                    self.move_diagnostics_panel(width, height, position, drag);
                    self.request_redraw();
                } else if self.text_drag.is_some() {
                    self.update_text_selection(position);
                    self.request_redraw();
                } else if self.wire_draft.is_some() {
                    self.update_wire_draft(width, height, position);
                    self.request_redraw();
                } else if let Some(drag) = &mut self.item_drag {
                    drag.current = position;
                    self.status = "Dragging · release to stage this edit".to_owned();
                    self.request_redraw();
                } else if let Some(selection) = &mut self.selection_box {
                    selection.current = position;
                    self.request_redraw();
                } else if self.panning {
                    if let Some(previous) = self.cursor {
                        self.pan.0 += position.x - previous.x;
                        self.pan.1 += position.y - previous.y;
                    }
                    self.request_redraw();
                }
                self.cursor = Some(position);
                self.request_redraw();
            }
            WindowEvent::CursorLeft { .. } => {
                self.finish_diagnostics_drag();
                self.cursor = None;
                self.panning = false;
                self.selection_box = None;
                self.item_drag = None;
                self.text_drag = None;
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Released {
                    if button == MouseButton::Left {
                        if self.diagnostics_drag.is_some() {
                            self.finish_diagnostics_drag();
                            self.request_redraw();
                        } else if self.text_drag.is_some() {
                            self.finish_text_selection();
                            self.request_redraw();
                        } else if let Some(drag) = self.item_drag.take() {
                            self.finish_item_drag(width, height, drag);
                            self.request_redraw();
                        } else if let Some(selection) = self.selection_box.take() {
                            self.finish_box_selection(width, height, selection);
                            self.request_redraw();
                        }
                    }
                    self.panning = false;
                    return;
                }
                let Some(cursor) = self.cursor else {
                    return;
                };
                if button == MouseButton::Left {
                    if self.handle_edit_controls(width, cursor.x, cursor.y)
                        || self.handle_toolbar(width, height, cursor.x, cursor.y)
                        || self.handle_diagnostics(width, height, cursor.x, cursor.y)
                    {
                        self.request_redraw();
                    } else if self.text_select_mode {
                        self.start_text_selection(cursor);
                        self.request_redraw();
                    } else if self.wire_draft.is_some() {
                        self.update_wire_draft(width, height, cursor);
                        self.commit_wire();
                        self.request_redraw();
                    } else if self.handle_filmstrip(width, height, cursor.x, cursor.y) {
                        self.request_redraw();
                    } else if self.main_rect(width, height).contains(cursor.x, cursor.y) {
                        let additive = self.modifiers.shift_key()
                            || self.modifiers.control_key()
                            || self.modifiers.super_key();
                        if self.select_at(width, height, cursor.x, cursor.y, additive) {
                            if self.edit_session.enabled
                                && !additive
                                && self.selected_items_are_movable()
                            {
                                self.start_item_drag(cursor);
                            }
                        } else {
                            if self.modifiers.alt_key() {
                                self.panning = true;
                            } else {
                                self.selection_box = Some(SelectionBox {
                                    start: cursor,
                                    current: cursor,
                                    additive,
                                });
                            }
                        }
                        self.request_redraw();
                    }
                } else if matches!(button, MouseButton::Middle | MouseButton::Right) {
                    self.panning = true;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(cursor) = self.cursor else {
                    return;
                };
                let amount = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        if self
                            .filmstrip_rect(width, height)
                            .contains(cursor.x, cursor.y)
                        {
                            f64::from(if x.abs() > y.abs() { x } else { y }) * 40.0
                        } else {
                            f64::from(y) * 40.0
                        }
                    }
                    MouseScrollDelta::PixelDelta(position) => {
                        if position.x.abs() > position.y.abs() {
                            position.x
                        } else {
                            position.y
                        }
                    }
                };
                if cursor.y >= height - TIMELINE_HEIGHT
                    && self
                        .filmstrip_rect(width, height)
                        .contains(cursor.x, cursor.y)
                {
                    const LABEL_WIDTH: f64 = 96.0;
                    const CARD_WIDTH: f64 = 138.0;
                    const CARD_GAP: f64 = 6.0;
                    let available = (width - SIDEBAR_WIDTH - LABEL_WIDTH - 20.0).max(CARD_WIDTH);
                    let visible = ((available + CARD_GAP) / (CARD_WIDTH + CARD_GAP))
                        .floor()
                        .max(1.0) as usize;
                    let maximum = self.timeline.len().saturating_sub(visible);
                    if amount > 0.0 {
                        self.timeline_scroll = (self.timeline_scroll + 1).min(maximum);
                    } else if amount < 0.0 {
                        self.timeline_scroll = self.timeline_scroll.saturating_sub(1);
                    }
                } else if self
                    .filmstrip_rect(width, height)
                    .contains(cursor.x, cursor.y)
                {
                    let content_width = self.sheets.len() as f64
                        * (THUMBNAIL_WIDTH + THUMBNAIL_GAP)
                        + THUMBNAIL_GAP;
                    self.film_scroll =
                        (self.film_scroll - amount).clamp(0.0, (content_width - width).max(0.0));
                } else {
                    self.set_zoom_about(
                        width,
                        height,
                        self.zoom * 1.0015_f64.powf(amount),
                        cursor.x,
                        cursor.y,
                    );
                }
                self.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let editing = self.modifiers.control_key() || self.modifiers.super_key();
                if editing
                    && matches!(event.logical_key.as_ref(), Key::Character(value) if value.eq_ignore_ascii_case("c"))
                    && self.text_selection.is_some()
                {
                    self.copy_selected_text();
                    self.request_redraw();
                    return;
                }
                if self.handle_sheet_pin_key(event.logical_key.as_ref()) {
                    self.request_redraw();
                    return;
                }
                if self.handle_sheet_key(event.logical_key.as_ref()) {
                    self.request_redraw();
                    return;
                }
                if self.handle_label_key(event.logical_key.as_ref()) {
                    self.request_redraw();
                    return;
                }
                if self.handle_property_key(event.logical_key.as_ref()) {
                    self.request_redraw();
                    return;
                }
                if self.handle_search_key(event.logical_key.as_ref()) {
                    self.request_redraw();
                    return;
                }
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Enter) if editing => self.commit_edit_session(),
                    Key::Named(NamedKey::Escape) => {
                        if self.text_select_mode || self.text_selection.is_some() {
                            self.text_select_mode = false;
                            self.text_drag = None;
                            self.text_selection = None;
                            self.status = self.live_status();
                        } else if self.external_preview.take().is_some() {
                            self.status = self.live_status();
                        } else {
                            let cancelled_wire = self.wire_draft.take().is_some();
                            self.selection_box = None;
                            self.item_drag = None;
                            self.panning = false;
                            if cancelled_wire {
                                self.status = "Wire cancelled".to_owned();
                            } else {
                                self.selected_uuids.clear();
                                self.status = self.live_status();
                            }
                        }
                    }
                    Key::Named(NamedKey::Delete) => self.delete_selected(),
                    Key::Named(NamedKey::ArrowLeft) if editing => {
                        self.nudge_selected(-self.grid_mm, 0.0);
                    }
                    Key::Named(NamedKey::ArrowRight) if editing => {
                        self.nudge_selected(self.grid_mm, 0.0);
                    }
                    Key::Named(NamedKey::ArrowUp) if editing => {
                        self.nudge_selected(0.0, -self.grid_mm);
                    }
                    Key::Named(NamedKey::ArrowDown) if editing => {
                        self.nudge_selected(0.0, self.grid_mm);
                    }
                    Key::Character(value)
                        if editing
                            && value.eq_ignore_ascii_case("z")
                            && self.modifiers.shift_key() =>
                    {
                        self.redo();
                    }
                    Key::Character(value) if editing && (value.eq_ignore_ascii_case("y")) => {
                        self.redo();
                    }
                    Key::Character(value) if editing && value.eq_ignore_ascii_case("z") => {
                        self.undo();
                    }
                    Key::Character(value) if editing && value.eq_ignore_ascii_case("f") => {
                        self.start_search();
                    }
                    Key::Character(value) if editing && value.eq_ignore_ascii_case("d") => {
                        self.duplicate_selected();
                    }
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.switch_sheet(self.active.saturating_sub(1));
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        self.switch_sheet(
                            (self.active + 1).min(self.sheets.len().saturating_sub(1)),
                        );
                    }
                    Key::Character("+" | "=") => {
                        let (x, y) = self.main_rect(width, height).center();
                        self.set_zoom_about(width, height, self.zoom * 1.25, x, y);
                    }
                    Key::Character("-") => {
                        let (x, y) = self.main_rect(width, height).center();
                        self.set_zoom_about(width, height, self.zoom / 1.25, x, y);
                    }
                    Key::Character("0") => self.fit(),
                    Key::Character(value) if value.eq_ignore_ascii_case("t") => {
                        self.toggle_theme();
                    }
                    Key::Character(value) if value.eq_ignore_ascii_case("g") => {
                        self.cycle_grid();
                    }
                    Key::Character(value) if value.eq_ignore_ascii_case("s") => {
                        self.toggle_snap();
                    }
                    Key::Character(value) if value.eq_ignore_ascii_case("e") => {
                        self.start_property_edit();
                    }
                    Key::Character(value) if !editing && value.eq_ignore_ascii_case("w") => {
                        self.start_wire(width, height, self.modifiers.shift_key());
                    }
                    Key::Character(value)
                        if !editing
                            && self.wire_draft.is_none()
                            && value.eq_ignore_ascii_case("j") =>
                    {
                        self.insert_at_cursor(width, height, "junction", |point| {
                            konnect_sexp::schematic::format_junction(point.x, point.y)
                        });
                    }
                    Key::Character(value)
                        if !editing
                            && self.wire_draft.is_none()
                            && value.eq_ignore_ascii_case("q") =>
                    {
                        self.insert_at_cursor(width, height, "no-connect marker", |point| {
                            konnect_sexp::schematic::format_no_connect(point.x, point.y)
                        });
                    }
                    Key::Character(value)
                        if !editing
                            && self.wire_draft.is_none()
                            && value.eq_ignore_ascii_case("l") =>
                    {
                        self.start_label_edit(width, height);
                    }
                    Key::Character(value)
                        if !editing
                            && self.wire_draft.is_none()
                            && value.eq_ignore_ascii_case("b") =>
                    {
                        if self.modifiers.shift_key() {
                            self.bus_entry_direction = self.bus_entry_direction.rotated_clockwise();
                            self.status =
                                format!("Bus-entry direction · {:?}", self.bus_entry_direction);
                        } else {
                            let direction = self.bus_entry_direction;
                            self.insert_at_cursor(width, height, "bus entry", |point| {
                                konnect_sexp::schematic::format_bus_entry(
                                    point.x, point.y, direction,
                                )
                            });
                        }
                    }
                    Key::Character(value)
                        if !editing
                            && self.wire_draft.is_none()
                            && value.eq_ignore_ascii_case("h") =>
                    {
                        self.start_sheet_edit(width, height);
                    }
                    Key::Character(value)
                        if !editing
                            && self.wire_draft.is_none()
                            && value.eq_ignore_ascii_case("p") =>
                    {
                        self.start_sheet_pin_edit(width, height);
                    }
                    Key::Character(value) if editing && value.eq_ignore_ascii_case("r") => {
                        if self.edit_session.has_pending() {
                            self.status = "Reload blocked · Commit or Discard staged changes first"
                                .to_owned();
                        } else {
                            let files = self
                                .sheets
                                .iter()
                                .map(|sheet| sheet.file.clone())
                                .collect::<Vec<_>>();
                            self.schedule_reload(&files);
                        }
                    }
                    Key::Character(value) if value.eq_ignore_ascii_case("r") => {
                        self.rotate_selected();
                    }
                    Key::Character(value) if value.eq_ignore_ascii_case("x") => {
                        self.mirror_selected("x");
                    }
                    Key::Character(value) if value.eq_ignore_ascii_case("y") => {
                        self.mirror_selected("y");
                    }
                    _ => {}
                }
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let Some((width, height, device_id, valid_surface)) =
                    self.state.as_ref().map(|state| {
                        (
                            state.surface.config.width,
                            state.surface.config.height,
                            state.surface.dev_id,
                            state.valid_surface,
                        )
                    })
                else {
                    return;
                };
                if !valid_surface {
                    return;
                }
                self.draw_frame(width, height);
                let Some(state) = &self.state else {
                    return;
                };
                let params = vello::RenderParams {
                    base_color: self.palette().app,
                    width,
                    height,
                    antialiasing_method: AaConfig::Area,
                };
                let Some(renderer) = self.renderers[device_id].as_mut() else {
                    return;
                };
                let device = &self.context.devices[device_id];
                if let Err(error) = renderer.render_to_texture(
                    &device.device,
                    &device.queue,
                    &self.frame,
                    &state.surface.target_view,
                    &params,
                ) {
                    self.status = format!("GPU render error: {error}");
                    return;
                }
                let texture = match state.surface.surface.get_current_texture() {
                    Ok(texture) => texture,
                    Err(error) => {
                        self.status = format!("Surface error: {error}");
                        return;
                    }
                };
                let mut encoder =
                    device
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("Konnect Vello blit"),
                        });
                state.surface.blitter.copy(
                    &device.device,
                    &mut encoder,
                    &state.surface.target_view,
                    &texture
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default()),
                );
                device.queue.submit([encoder.finish()]);
                texture.present();
                let _ = device.device.poll(wgpu::PollType::Poll);
                if self.timeline.is_animating(Instant::now()) {
                    self.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::FilesChanged(paths) => {
                let paths = self.external_paths(paths);
                if paths.is_empty() {
                    return;
                }
                let keys = paths.iter().map(|path| path_key(path)).collect::<Vec<_>>();
                let conflicts = self.edit_session.mark_external_change(keys.clone());
                let paths = paths
                    .into_iter()
                    .zip(keys)
                    .filter_map(|(path, key)| {
                        self.edit_session
                            .staged_source(&key)
                            .is_none()
                            .then_some(path)
                    })
                    .collect::<Vec<_>>();
                if conflicts > 0 {
                    self.status = format!(
                        "External change overlaps {conflicts} staged file(s) · Commit blocked; Discard to load it"
                    );
                }
                if conflicts == 0 {
                    self.status = "Change detected · rendering directly…".to_owned();
                }
                if !paths.is_empty() {
                    self.schedule_external_reload(&paths);
                }
                self.request_redraw();
            }
            UserEvent::Reloaded(batch) => {
                self.apply_reload_batch(batch);
                self.request_redraw();
            }
        }
    }
}

fn spawn_reload_worker(
    receiver: mpsc::Receiver<ReloadRequest>,
    proxy: EventLoopProxy<UserEvent>,
    latest_generation: Arc<AtomicU64>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("schematic-reload".to_owned())
        .spawn(move || {
            let mut dirty = HashSet::new();
            while let Ok(mut request) = receiver.recv() {
                let mut external = request.external;
                dirty.extend(request.changed.iter().cloned());
                while let Ok(next) = receiver.try_recv() {
                    external |= next.external;
                    dirty.extend(next.changed.iter().cloned());
                    request = next;
                }
                request.changed.clone_from(&dirty);
                request.external = external;

                let Some(batch) = build_reload_batch(&request, &latest_generation) else {
                    continue;
                };
                if proxy.send_event(UserEvent::Reloaded(batch)).is_err() {
                    break;
                }
                dirty.clear();
            }
        })
        .context("failed to start schematic reload worker")?;
    Ok(())
}

fn build_reload_batch(
    request: &ReloadRequest,
    latest_generation: &AtomicU64,
) -> Option<ReloadBatch> {
    let entries = match discover_hierarchy(&request.root) {
        Ok(entries) => entries,
        Err(error) => {
            return (latest_generation.load(Ordering::Acquire) == request.generation).then(|| {
                ReloadBatch {
                    generation: request.generation,
                    entries: Err(format!("{error:#}")),
                    loaded: HashMap::new(),
                    external: request.external,
                }
            });
        }
    };
    if latest_generation.load(Ordering::Acquire) != request.generation {
        return None;
    }

    let mut loaded = HashMap::new();
    for entry in &entries {
        let key = path_key(&entry.file);
        if !request.changed.contains(&key) && request.known.contains(&key) {
            continue;
        }
        if latest_generation.load(Ordering::Acquire) != request.generation {
            return None;
        }
        loaded.insert(
            key,
            load_scene_with_fallback(&request.root, &entries, &entry.file),
        );
    }
    (latest_generation.load(Ordering::Acquire) == request.generation).then_some(ReloadBatch {
        generation: request.generation,
        entries: Ok(entries),
        loaded,
        external: request.external,
    })
}

fn load_scene_with_fallback(
    root: &Path,
    entries: &[HierarchyEntry],
    file: &Path,
) -> std::result::Result<LoadedScene, String> {
    let semantic = SchematicScene::load(file).map_err(|error| format!("{error:#}"))?;
    if semantic.coverage.is_complete() {
        return Ok(LoadedScene {
            semantic,
            compatibility: None,
            compatibility_error: None,
        });
    }
    match crate::compat_svg::load_or_export(root, entries, file) {
        Ok(svg) => match compatibility_scene(&svg) {
            Ok(compatibility) => Ok(LoadedScene {
                semantic,
                compatibility: Some(compatibility),
                compatibility_error: None,
            }),
            Err(error) => Ok(LoadedScene {
                semantic,
                compatibility: None,
                compatibility_error: Some(format!("KiCad SVG parse failed: {error:#}")),
            }),
        },
        Err(error) => Ok(LoadedScene {
            semantic,
            compatibility: None,
            compatibility_error: Some(error),
        }),
    }
}

pub(crate) fn run() -> Result<()> {
    #[cfg(feature = "golden-svg-reference")]
    if let Some((output, width, height, svg)) = render_svg_png_argument()? {
        return render_svg_png(&svg, &output, width, height);
    }
    let root = schematic_argument()?;
    let journal_root = root
        .parent()
        .ok_or_else(|| anyhow!("the root schematic has no project directory"))?;
    let recovered = recover_file_transactions(journal_root).with_context(|| {
        format!(
            "failed to recover transactions in {}",
            journal_root.display()
        )
    })?;
    if let Some(iterations) = benchmark_iterations_argument()? {
        return benchmark_scene_pipeline(&root, iterations);
    }
    if let Some(output) = render_png_argument() {
        return render_png(&root, &output, 10.0);
    }
    let font = NativeFont::load()?;
    let hierarchy = load_hierarchy(&root)?;
    let palette = Theme::Light.palette();
    let sheets = hierarchy
        .into_iter()
        .map(|entry| NativeSheet::from_hierarchy(entry, palette))
        .collect::<Vec<_>>();
    if sheets.is_empty() {
        return Err(anyhow!("the schematic hierarchy is empty"));
    }

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("failed to create native event loop")?;
    let proxy = event_loop.create_proxy();
    let reload_generation = Arc::new(AtomicU64::new(0));
    let (reload_tx, reload_rx) = mpsc::channel();
    spawn_reload_worker(reload_rx, proxy.clone(), Arc::clone(&reload_generation))?;
    let callback_proxy = proxy.clone();
    let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            return;
        };
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        let paths = event
            .paths
            .into_iter()
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "kicad_sch")
            })
            .collect::<Vec<_>>();
        if !paths.is_empty() {
            let _ = callback_proxy.send_event(UserEvent::FilesChanged(paths));
        }
    })
    .context("failed to create schematic watcher")?;

    let mut viewer = VelloViewer::new(root, font, sheets, watcher, reload_tx, reload_generation);
    if !recovered.is_empty() {
        let files = recovered
            .iter()
            .map(|outcome| outcome.completed_files)
            .sum::<usize>();
        viewer.status = format!(
            "Recovered {} interrupted transaction(s), completing {files} file(s)",
            recovered.len()
        );
    }
    let initial_files = viewer
        .sheets
        .iter()
        .map(|sheet| sheet.file.clone())
        .collect::<Vec<_>>();
    viewer.schedule_reload(&initial_files);
    event_loop
        .run_app(&mut viewer)
        .context("native event loop failed")
}

#[cfg(feature = "golden-svg-reference")]
fn render_svg_png(svg_path: &Path, output: &Path, width: u32, height: u32) -> Result<()> {
    const SVG_PX_TO_MM: f64 = 25.4 / 96.0;

    let source = std::fs::read_to_string(svg_path)
        .with_context(|| format!("failed to read {}", svg_path.display()))?;
    let options = vello_svg::usvg::Options::default();
    let tree = vello_svg::usvg::Tree::from_str(&source, &options)
        .with_context(|| format!("failed to parse {}", svg_path.display()))?;
    let mut reference = Scene::new();
    append_svg_group_flat(&mut reference, tree.root());
    let size = tree.size();
    let scale_x = f64::from(width) / (f64::from(size.width()) * SVG_PX_TO_MM);
    let scale_y = f64::from(height) / (f64::from(size.height()) * SVG_PX_TO_MM);
    // The native golden renderer uses a fixed physical scale and rounds the
    // output extent to whole pixels. Do the same here instead of stretching
    // the slightly-over-nominal KiCad paper dimensions independently in X/Y.
    let pixels_per_mm = ((scale_x + scale_y) / 2.0).round().max(1.0);
    let mut scene = Scene::new();
    scene.append(&reference, Some(Affine::scale(pixels_per_mm)));
    pollster::block_on(render_scene_png(
        &scene,
        output,
        width,
        height,
        Theme::Light.palette().page,
    ))
}

fn compatibility_scene(source: &str) -> Result<Scene> {
    let options = vello_svg::usvg::Options::default();
    let tree = vello_svg::usvg::Tree::from_str(source, &options)
        .context("failed to parse KiCad compatibility SVG")?;
    let mut scene = Scene::new();
    append_svg_group_flat(&mut scene, tree.root());
    Ok(scene)
}

fn append_svg_group_flat(scene: &mut Scene, group: &vello_svg::usvg::Group) {
    const SVG_PX_TO_MM: f64 = 25.4 / 96.0;
    static PATH_INDEX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    for node in group.children() {
        match node {
            vello_svg::usvg::Node::Group(group) => append_svg_group_flat(scene, group),
            vello_svg::usvg::Node::Path(path) if path.is_visible() => {
                let geometry = vello_svg::util::to_bez_path(path);
                let transform = canonical_svg_transform(
                    Affine::scale(SVG_PX_TO_MM) * vello_svg::util::to_affine(&path.abs_transform()),
                );
                let path_index = PATH_INDEX.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if std::env::var_os("KONNECT_SVG_STATS").is_some() && path_index < 12 {
                    eprintln!(
                        "svg path {path_index}: bbox={:?} transform={transform:?} stroke={:?}",
                        path.bounding_box(),
                        path.stroke().map(|stroke| stroke.width())
                    );
                }
                match path.paint_order() {
                    vello_svg::usvg::PaintOrder::FillAndStroke => {
                        append_svg_fill(scene, path, &geometry, transform);
                        append_svg_stroke(scene, path, &geometry, transform);
                    }
                    vello_svg::usvg::PaintOrder::StrokeAndFill => {
                        append_svg_stroke(scene, path, &geometry, transform);
                        append_svg_fill(scene, path, &geometry, transform);
                    }
                }
            }
            // KiCad emits invisible SVG text for search/accessibility and
            // follows it with the authoritative visible Newstroke paths.
            // Flattening the invisible text loses its group opacity and
            // renders a duplicate, sometimes at an untransformed origin.
            vello_svg::usvg::Node::Text(_)
            | vello_svg::usvg::Node::Path(_)
            | vello_svg::usvg::Node::Image(_) => {}
        }
    }
}

fn canonical_svg_transform(transform: Affine) -> Affine {
    let mut coefficients = transform.as_coeffs();
    for (index, coefficient) in coefficients.iter_mut().enumerate() {
        let identity = matches!(index, 0 | 3);
        let target = if identity { 1.0 } else { 0.0 };
        if (*coefficient - target).abs() < 1e-5 {
            *coefficient = target;
        }
    }
    Affine::new(coefficients)
}

fn append_svg_fill(
    scene: &mut Scene,
    path: &vello_svg::usvg::Path,
    geometry: &BezPath,
    transform: Affine,
) {
    let Some(fill) = path.fill() else {
        return;
    };
    let Some((brush, brush_transform)) = vello_svg::util::to_brush(fill.paint(), fill.opacity())
    else {
        return;
    };
    scene.fill(
        match fill.rule() {
            vello_svg::usvg::FillRule::NonZero => Fill::NonZero,
            vello_svg::usvg::FillRule::EvenOdd => Fill::EvenOdd,
        },
        transform,
        &brush,
        Some(brush_transform),
        geometry,
    );
}

fn append_svg_stroke(
    scene: &mut Scene,
    path: &vello_svg::usvg::Path,
    geometry: &BezPath,
    transform: Affine,
) {
    let Some(stroke) = path.stroke() else {
        return;
    };
    let Some((brush, brush_transform)) =
        vello_svg::util::to_brush(stroke.paint(), stroke.opacity())
    else {
        return;
    };
    scene.stroke(
        &vello_svg::util::to_stroke(stroke),
        transform,
        &brush,
        Some(brush_transform),
        geometry,
    );
}

fn render_png(schematic: &Path, output: &Path, pixels_per_mm: f64) -> Result<()> {
    let palette = Theme::Light.palette();
    let semantic = SchematicScene::load(schematic)?;
    if std::env::var_os("KONNECT_SCENE_STATS").is_some() {
        let geometry = semantic
            .primitives
            .iter()
            .filter(|primitive| !matches!(primitive, Primitive::Text { .. }))
            .count();
        eprintln!("scene geometry: {geometry} primitives");
        for role in [
            ColorRole::Border,
            ColorRole::Bus,
            ColorRole::GraphicText,
            ColorRole::Junction,
            ColorRole::Label,
            ColorRole::NoConnect,
            ColorRole::Page,
            ColorRole::Pin,
            ColorRole::PinName,
            ColorRole::PinNumber,
            ColorRole::SheetFile,
            ColorRole::Symbol,
            ColorRole::Text,
            ColorRole::Wire,
        ] {
            let texts = semantic
                .primitives
                .iter()
                .filter_map(|primitive| match primitive {
                    Primitive::Text {
                        role: primitive_role,
                        text,
                        ..
                    } if *primitive_role == role => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            eprintln!(
                "scene {role:?}: {} runs, {} characters",
                texts.len(),
                texts.iter().map(|text| text.chars().count()).sum::<usize>()
            );
        }
    }
    if std::env::var_os("KONNECT_SCENE_TEXTS").is_some() {
        for primitive in &semantic.primitives {
            if let Primitive::Text {
                position,
                rotation_deg,
                size_mm,
                stroke_width_mm,
                align,
                italic,
                role,
                text,
            } = primitive
            {
                eprintln!(
                    "scene text {role:?}: {text:?} at=({:.4},{:.4}) rotation={rotation_deg:.1} size={size_mm:.4} stroke={stroke_width_mm:.4} align={align:?} italic={italic}",
                    position.x, position.y
                );
            }
        }
    }
    if std::env::var_os("KONNECT_SCENE_OBJECTS").is_some() {
        for object in &semantic.objects {
            eprintln!(
                "scene object {:?} {} {} {:.4},{:.4}..{:.4},{:.4} index={:.4},{:.4}..{:.4},{:.4} initial={:.4},{:.4}..{:.4},{:.4}",
                object.kind,
                object.label,
                object.uuid,
                object.bounds.min_x,
                object.bounds.min_y,
                object.bounds.max_x,
                object.bounds.max_y,
                object.index_bounds.min_x,
                object.index_bounds.min_y,
                object.index_bounds.max_x,
                object.index_bounds.max_y,
                object.initial_index_bounds.min_x,
                object.initial_index_bounds.min_y,
                object.initial_index_bounds.max_x,
                object.initial_index_bounds.max_y
            );
        }
    }
    let width = (semantic.width_mm * pixels_per_mm).round() as u32;
    let height = (semantic.height_mm * pixels_per_mm).round() as u32;
    let sheet = NativeSheet {
        name: schematic
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("schematic")
            .to_owned(),
        depth: 0,
        file: schematic.to_path_buf(),
        rendered: encode_scene(&semantic, palette),
        semantic,
        compatibility: None,
        compatibility_error: None,
    };
    let mut scene = Scene::new();
    append_sheet(&mut scene, &sheet, Affine::scale(pixels_per_mm));

    pollster::block_on(render_scene_png(
        &scene,
        output,
        width,
        height,
        palette.page,
    ))
}

async fn render_scene_png(
    scene: &Scene,
    output: &Path,
    width: u32,
    height: u32,
    background: Color,
) -> Result<()> {
    let mut context = RenderContext::new();
    let device_id = context
        .device(None)
        .await
        .ok_or_else(|| anyhow!("no compatible GPU adapter is available"))?;
    let handle = &context.devices[device_id];
    let texture = handle.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Konnect schematic golden image"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = Renderer::new(
        &handle.device,
        RendererOptions {
            // Headless goldens must be byte-stable.  The GPU path can vary a
            // single 8-bit channel at a primitive overlap across otherwise
            // identical runs; Vello's CPU path removes that race.  The live
            // viewer above remains GPU-accelerated.
            use_cpu: true,
            antialiasing_support: [AaConfig::Area].into_iter().collect(),
            num_init_threads: None,
            pipeline_cache: None,
        },
    )?;
    renderer.render_to_texture(
        &handle.device,
        &handle.queue,
        scene,
        &view,
        &vello::RenderParams {
            base_color: background,
            width,
            height,
            antialiasing_method: AaConfig::Area,
        },
    )?;

    let unpadded_bytes_per_row = width * 4;
    let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(256) * 256;
    let buffer = handle.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Konnect schematic PNG readback"),
        size: u64::from(padded_bytes_per_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = handle
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Konnect schematic PNG copy"),
        });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    handle.queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    handle.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    })?;
    receiver
        .recv()
        .context("GPU readback callback was dropped")?
        .context("failed to map the rendered image")?;
    let mapped = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for row in mapped.chunks_exact(padded_bytes_per_row as usize) {
        pixels.extend_from_slice(&row[..unpadded_bytes_per_row as usize]);
    }

    let file = std::fs::File::create(output)
        .with_context(|| format!("failed to create {}", output.display()))?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(&pixels)
        .with_context(|| format!("failed to write {}", output.display()))?;
    Ok(())
}

fn schematic_argument() -> Result<PathBuf> {
    let path = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .find(|argument| {
            argument
                .extension()
                .is_some_and(|extension| extension == "kicad_sch")
        })
        .ok_or_else(|| anyhow!("usage: schematic-viewer <project.kicad_sch>"))?;
    if !path.is_file() {
        return Err(anyhow!("schematic not found: {}", path.display()));
    }
    Ok(path.canonicalize().unwrap_or(path))
}

fn render_png_argument() -> Option<PathBuf> {
    let mut arguments = std::env::args_os();
    while let Some(argument) = arguments.next() {
        if argument == "--render-png" {
            return arguments.next().map(PathBuf::from);
        }
    }
    None
}

fn benchmark_iterations_argument() -> Result<Option<usize>> {
    let mut arguments = std::env::args_os();
    while let Some(argument) = arguments.next() {
        if argument == "--benchmark-load" {
            let iterations = arguments
                .next()
                .and_then(|value| value.to_str().and_then(|value| value.parse::<usize>().ok()))
                .ok_or_else(|| anyhow!("--benchmark-load requires a positive iteration count"))?;
            if iterations == 0 {
                return Err(anyhow!("--benchmark-load iteration count must be non-zero"));
            }
            return Ok(Some(iterations));
        }
    }
    Ok(None)
}

fn benchmark_scene_pipeline(root: &Path, iterations: usize) -> Result<()> {
    let palette = Theme::Light.palette();
    // Warm filesystem metadata, font tables, and allocator paths before the
    // measured samples. The benchmark intentionally excludes GPU submission:
    // it measures the parse/semantic/scene-encoding work that gates reloads.
    let warm = load_hierarchy(root)?;
    std::hint::black_box(
        warm.into_iter()
            .map(|entry| NativeSheet::from_hierarchy(entry, palette))
            .collect::<Vec<_>>(),
    );

    let mut hierarchy_ms = Vec::with_capacity(iterations);
    let mut active_sheet_ms = Vec::with_capacity(iterations);
    let mut pages = 0usize;
    for _ in 0..iterations {
        let started = std::time::Instant::now();
        let hierarchy = load_hierarchy(root)?;
        pages = hierarchy.len();
        let encoded = hierarchy
            .into_iter()
            .map(|entry| NativeSheet::from_hierarchy(entry, palette))
            .collect::<Vec<_>>();
        std::hint::black_box(encoded);
        hierarchy_ms.push(started.elapsed().as_secs_f64() * 1_000.0);

        let started = std::time::Instant::now();
        let scene = SchematicScene::load(root)?;
        let encoded = encode_scene(&scene, palette);
        std::hint::black_box(encoded);
        active_sheet_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    let hierarchy = latency_summary(&mut hierarchy_ms);
    let active = latency_summary(&mut active_sheet_ms);
    println!(
        "KONNECT_BENCH pages={pages} iterations={iterations} hierarchy_mean_ms={:.3} hierarchy_p95_ms={:.3} hierarchy_max_ms={:.3} active_mean_ms={:.3} active_p95_ms={:.3} active_max_ms={:.3}",
        hierarchy.mean_ms,
        hierarchy.p95_ms,
        hierarchy.max_ms,
        active.mean_ms,
        active.p95_ms,
        active.max_ms,
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LatencySummary {
    mean_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

fn latency_summary(samples: &mut [f64]) -> LatencySummary {
    samples.sort_by(f64::total_cmp);
    let mean_ms = samples.iter().sum::<f64>() / samples.len() as f64;
    let p95_index = ((samples.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(samples.len().saturating_sub(1));
    LatencySummary {
        mean_ms,
        p95_ms: samples[p95_index],
        max_ms: *samples.last().unwrap_or(&0.0),
    }
}

#[cfg(feature = "golden-svg-reference")]
fn render_svg_png_argument() -> Result<Option<(PathBuf, u32, u32, PathBuf)>> {
    let mut arguments = std::env::args_os();
    while let Some(argument) = arguments.next() {
        if argument == "--render-svg-png" {
            let output = arguments
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("--render-svg-png requires OUTPUT WIDTH HEIGHT SVG"))?;
            let width = arguments
                .next()
                .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
                .ok_or_else(|| anyhow!("--render-svg-png WIDTH must be a positive integer"))?;
            let height = arguments
                .next()
                .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
                .ok_or_else(|| anyhow!("--render-svg-png HEIGHT must be a positive integer"))?;
            let svg = arguments
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("--render-svg-png requires an SVG path"))?;
            if width == 0 || height == 0 {
                return Err(anyhow!("--render-svg-png dimensions must be non-zero"));
            }
            if !svg.is_file() {
                return Err(anyhow!("SVG not found: {}", svg.display()));
            }
            return Ok(Some((output, width, height, svg)));
        }
    }
    Ok(None)
}

fn window_attributes() -> WindowAttributes {
    Window::default_attributes()
        .with_inner_size(LogicalSize::new(1280.0, 900.0))
        .with_min_inner_size(LogicalSize::new(720.0, 480.0))
        .with_resizable(true)
        .with_window_icon(window_icon())
        .with_title("Konnect — Schematic Studio")
}

fn window_icon() -> Option<Icon> {
    let decoder = png::Decoder::new(Cursor::new(include_bytes!(
        "../../../packaging/resources/icon.png"
    )));
    let mut reader = decoder.read_info().ok()?;
    let mut pixels = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut pixels).ok()?;
    Icon::from_rgba(
        pixels[..info.buffer_size()].to_vec(),
        info.width,
        info.height,
    )
    .ok()
}

fn path_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn sheet_rectangle(source: &str, uuid: &str) -> Option<(f64, f64, f64, f64)> {
    konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find_map(|(start, end)| {
            let node = parse_sexp(&source[start..end]).ok()?;
            if node.head() != Some("sheet") || node.find_str("uuid") != Some(uuid) {
                return None;
            }
            let (x, y, _) = konnect_sexp::schematic::parse_at(&node)?;
            let size = node.find("size")?;
            Some((x, y, x + size.get_f64(1)?, y + size.get_f64(2)?))
        })
}

fn nearest_sheet_edge(
    rectangle: (f64, f64, f64, f64),
    cursor: SchPoint,
    grid_mm: f64,
    snap_enabled: bool,
) -> (SchPoint, f64) {
    let (left, top, right, bottom) = rectangle;
    let snapped = snap_point(cursor, snap_enabled, grid_mm);
    let distances = [
        ((cursor.x - right).abs(), 0.0),
        ((cursor.x - left).abs(), 180.0),
        ((cursor.y - top).abs(), 90.0),
        ((cursor.y - bottom).abs(), 270.0),
    ];
    let rotation = distances
        .into_iter()
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .map_or(0.0, |(_, rotation)| rotation);
    let point = match rotation as u16 {
        0 => SchPoint {
            x: right,
            y: snapped.y.clamp(top, bottom),
        },
        180 => SchPoint {
            x: left,
            y: snapped.y.clamp(top, bottom),
        },
        90 => SchPoint {
            x: snapped.x.clamp(left, right),
            y: top,
        },
        _ => SchPoint {
            x: snapped.x.clamp(left, right),
            y: bottom,
        },
    };
    (point, rotation)
}

fn next_sheet_instance(source: &str) -> Result<(String, String)> {
    let root = parse_sexp(source).context("parent schematic is not valid S-expression")?;
    let root_uuid = root
        .find("uuid")
        .and_then(|uuid| uuid.get(1))
        .and_then(konnect_sexp::SexpNode::as_str)
        .filter(|uuid| !uuid.is_empty())
        .context("parent schematic has no root UUID")?;
    let mut max_page = 1_u32;
    for sheet in root.find_all("sheet") {
        let Some(instances) = sheet.find("instances") else {
            continue;
        };
        for project in instances.find_all("project") {
            for path in project.find_all("path") {
                if let Some(page) = path
                    .find("page")
                    .and_then(|page| page.get(1))
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .and_then(|page| page.parse::<u32>().ok())
                {
                    max_page = max_page.max(page);
                }
            }
        }
    }
    Ok((format!("/{root_uuid}"), (max_page + 1).to_string()))
}

fn truncate_ui(value: &str, max_chars: usize) -> String {
    let mut characters = value.chars();
    let head = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn display_sheet_name(value: &str) -> String {
    let value = value
        .split_once('_')
        .filter(|(prefix, _)| prefix.chars().all(|character| character.is_ascii_digit()))
        .map_or(value, |(_, rest)| rest);
    let words = value
        .split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| match word.to_ascii_uppercase().as_str() {
            "BMS" | "AFE" | "MCU" | "USB" | "I2C" | "SPI" | "CAN" => word.to_ascii_uppercase(),
            _ if word.chars().any(|character| character.is_ascii_digit()) => {
                word.to_ascii_uppercase()
            }
            _ => {
                let lowercase = word.to_ascii_lowercase();
                let mut characters = lowercase.chars();
                characters
                    .next()
                    .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        value.to_owned()
    } else {
        words.join(" ")
    }
}

fn union_bounds(left: Bounds, right: Bounds) -> Bounds {
    Bounds {
        min_x: left.min_x.min(right.min_x),
        min_y: left.min_y.min(right.min_y),
        max_x: left.max_x.max(right.max_x),
        max_y: left.max_y.max(right.max_y),
    }
}

fn relative_time(recorded_at: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(recorded_at)
        .unwrap_or(Duration::ZERO);
    match elapsed.as_secs() {
        0..=4 => "now".to_owned(),
        seconds @ 5..=59 => format!("{seconds}s"),
        seconds @ 60..=3_599 => format!("{}m", seconds / 60),
        seconds => format!("{}h", seconds / 3_600),
    }
}

fn push_history(stack: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    if stack.len() == HISTORY_LIMIT {
        stack.remove(0);
    }
    stack.push(entry);
}

fn append_sheet(frame: &mut Scene, sheet: &NativeSheet, transform: Affine) {
    if !sheet.semantic.coverage.is_complete() {
        if let Some(compatibility) = &sheet.compatibility {
            frame.append(compatibility, Some(transform));
            return;
        }
    }
    frame.append(&sheet.rendered, Some(transform));
}

fn append_object_artwork(
    frame: &mut Scene,
    sheet: &NativeSheet,
    object: &crate::native_scene::SceneObject,
    transform: Affine,
    palette: Palette,
) {
    let Some(primitives) = sheet
        .semantic
        .primitives
        .get(object.primitive_range.clone())
    else {
        return;
    };
    frame.append(&encode_primitives(primitives, palette), Some(transform));
}

fn append_selection(
    frame: &mut Scene,
    sheet: &NativeSheet,
    object: &crate::native_scene::SceneObject,
    transform: Affine,
    scale: f64,
    palette: Palette,
) {
    let Some(primitives) = sheet
        .semantic
        .primitives
        .get(object.primitive_range.clone())
    else {
        return;
    };
    let halo_mm = 3.0 / scale.max(0.001);
    let color = palette.selection.with_alpha(0.76);
    let mut outline = Scene::new();

    for primitive in primitives {
        match primitive {
            Primitive::Line { from, to, style } => outline.stroke(
                &Stroke::new(style.width_mm.max(0.05) + halo_mm),
                Affine::IDENTITY,
                color,
                None,
                &Line::new((from.x, from.y), (to.x, to.y)),
            ),
            Primitive::Polyline {
                points,
                closed,
                style,
                ..
            } => outline.stroke(
                &Stroke::new(style.width_mm.max(0.05) + halo_mm),
                Affine::IDENTITY,
                color,
                None,
                &polyline_path(points, *closed),
            ),
            Primitive::Rect { bounds, style, .. } => outline.stroke(
                &Stroke::new(style.width_mm.max(0.05) + halo_mm),
                Affine::IDENTITY,
                color,
                None,
                &Rect::new(bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y),
            ),
            Primitive::Circle {
                center,
                radius,
                style,
                ..
            } => outline.stroke(
                &Stroke::new(style.width_mm.max(0.05) + halo_mm),
                Affine::IDENTITY,
                color,
                None,
                &Circle::new((center.x, center.y), *radius),
            ),
            Primitive::Arc {
                start,
                mid,
                end,
                style,
            } => {
                if let Some(arc) = arc_shape(*start, *mid, *end) {
                    outline.stroke(
                        &round_stroke(style.width_mm.max(0.05) + halo_mm),
                        Affine::IDENTITY,
                        color,
                        None,
                        &arc,
                    );
                }
            }
            Primitive::Bezier { points, style } => {
                let mut path = BezPath::new();
                if let Some(first) = points.first() {
                    path.move_to((first.x, first.y));
                    for controls in points[1..].chunks_exact(3) {
                        path.curve_to(
                            (controls[0].x, controls[0].y),
                            (controls[1].x, controls[1].y),
                            (controls[2].x, controls[2].y),
                        );
                    }
                    outline.stroke(
                        &Stroke::new(style.width_mm.max(0.05) + halo_mm),
                        Affine::IDENTITY,
                        color,
                        None,
                        &path,
                    );
                }
            }
            Primitive::Text { .. } => {}
        }
    }
    frame.append(&outline, Some(transform));
}

#[cfg(test)]
fn arc_points(start: SchPoint, mid: SchPoint, end: SchPoint, segments: usize) -> Vec<SchPoint> {
    let Some(arc) = arc_shape(start, mid, end) else {
        return vec![start, mid, end];
    };
    (0..=segments)
        .map(|index| {
            let angle = arc.start_angle + arc.sweep_angle * index as f64 / segments as f64;
            SchPoint {
                x: arc.center.x + arc.radii.x * angle.cos(),
                y: arc.center.y + arc.radii.y * angle.sin(),
            }
        })
        .collect()
}

fn arc_shape(start: SchPoint, mid: SchPoint, end: SchPoint) -> Option<KurboArc> {
    let determinant =
        2.0 * (start.x * (mid.y - end.y) + mid.x * (end.y - start.y) + end.x * (start.y - mid.y));
    if determinant.abs() < 1e-9 {
        return None;
    }
    let start_sq = start.x * start.x + start.y * start.y;
    let mid_sq = mid.x * mid.x + mid.y * mid.y;
    let end_sq = end.x * end.x + end.y * end.y;
    let center_x =
        (start_sq * (mid.y - end.y) + mid_sq * (end.y - start.y) + end_sq * (start.y - mid.y))
            / determinant;
    let center_y =
        (start_sq * (end.x - mid.x) + mid_sq * (start.x - end.x) + end_sq * (mid.x - start.x))
            / determinant;
    let radius = (start.x - center_x).hypot(start.y - center_y);
    let start_angle = (start.y - center_y).atan2(start.x - center_x);
    let mid_angle = (mid.y - center_y).atan2(mid.x - center_x);
    let end_angle = (end.y - center_y).atan2(end.x - center_x);
    let ccw = positive_angle(end_angle - start_angle);
    let mid_ccw = positive_angle(mid_angle - start_angle);
    let sweep = if mid_ccw <= ccw + 1e-9 {
        ccw
    } else {
        ccw - std::f64::consts::TAU
    };
    Some(KurboArc::new(
        (center_x, center_y),
        (radius, radius),
        start_angle,
        sweep,
        0.0,
    ))
}

fn positive_angle(angle: f64) -> f64 {
    angle.rem_euclid(std::f64::consts::TAU)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relative_luminance(color: Color) -> f32 {
        let linear = |component: f32| {
            if component <= 0.040_45 {
                component / 12.92
            } else {
                ((component + 0.055) / 1.055).powf(2.4)
            }
        };
        let [red, green, blue, _] = color.components;
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }

    fn contrast_ratio(left: Color, right: Color) -> f32 {
        let left = relative_luminance(left);
        let right = relative_luminance(right);
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    }

    #[test]
    fn arc_interpolation_contains_endpoints() {
        let start = SchPoint { x: 1.0, y: 0.0 };
        let mid = SchPoint { x: 0.0, y: 1.0 };
        let end = SchPoint { x: -1.0, y: 0.0 };
        let points = arc_points(start, mid, end, 8);
        assert_eq!(points.first(), Some(&start));
        let last = points.last().unwrap();
        assert!((last.x - end.x).abs() < 1e-9);
        assert!((last.y - end.y).abs() < 1e-9);
    }

    #[test]
    fn matches_kicad_inductor_arc_serialization() {
        let arc = kicad_svg_arc(
            SchPoint { x: 98.30, y: 96.53 },
            SchPoint { x: 97.29, y: 95.50 },
            SchPoint { x: 96.27, y: 96.51 },
        )
        .unwrap();

        assert_eq!(arc.from.x, 98.31);
        assert_eq!(arc.from.y, 96.5301);
        assert_eq!(arc.to.x, 96.27);
        assert_eq!(arc.to.y, 96.51);
        assert_eq!(arc.radii.x, 1.02);
        assert!(arc.large_arc);
        assert!(!arc.sweep);

        let path = svg_arc_path(
            SchPoint { x: 98.30, y: 96.53 },
            SchPoint { x: 97.29, y: 95.50 },
            SchPoint { x: 96.27, y: 96.51 },
        )
        .unwrap();
        assert_eq!(
            path.elements()[1],
            vello::kurbo::PathEl::CurveTo(
                vello::kurbo::Point::new(98.315_551_757_812_5, 95.966_766_357_421_88),
                vello::kurbo::Point::new(97.863_380_432_128_9, 95.505_599_975_585_94),
                vello::kurbo::Point::new(97.300_048_828_125, 95.500_053_405_761_72),
            )
        );
    }

    #[test]
    fn toolbar_buttons_remain_inside_window() {
        for (_, rect) in VelloViewer::toolbar_buttons(1280.0) {
            assert!(rect.x0 >= 0.0);
            assert!(rect.x1 <= 1280.0);
            assert!(rect.y0 >= STATUS_HEIGHT);
            assert!(rect.y1 <= 480.0);
        }
    }

    #[test]
    fn every_toolbar_and_change_icon_encodes_without_panicking() {
        let mut scene = Scene::new();
        for icon in [
            UiIcon::Add,
            UiIcon::Commit,
            UiIcon::Delete,
            UiIcon::Discard,
            UiIcon::Duplicate,
            UiIcon::Edit,
            UiIcon::External,
            UiIcon::Fit,
            UiIcon::Follow,
            UiIcon::Grid,
            UiIcon::Highlight,
            UiIcon::Move,
            UiIcon::Redo,
            UiIcon::Scale,
            UiIcon::Snap,
            UiIcon::TextSelect,
            UiIcon::Theme,
            UiIcon::Transform,
            UiIcon::Undo,
            UiIcon::Wire,
            UiIcon::ZoomIn,
            UiIcon::ZoomOut,
        ] {
            draw_ui_icon(
                &mut scene,
                icon,
                ScreenRect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 32.0,
                    y1: 32.0,
                },
                Theme::Dark.palette().text,
            );
        }
    }

    #[test]
    fn edit_controls_are_compact_and_inside_the_minimum_window() {
        let controls = VelloViewer::edit_controls(720.0);

        assert_eq!(controls.len(), 3);
        assert!(controls.iter().all(|(_, rect)| {
            rect.x0 >= SIDEBAR_WIDTH
                && rect.x1 <= 720.0
                && rect.y0 >= 0.0
                && rect.y1 <= STATUS_HEIGHT
        }));
        assert!(controls.windows(2).all(|pair| pair[0].1.x1 < pair[1].1.x0));
    }

    #[test]
    fn selectable_text_extracts_unicode_ranges_without_byte_slicing() {
        let target = SelectableText {
            text: "AΩB".to_owned(),
            rect: ScreenRect {
                x0: 0.0,
                y0: 0.0,
                x1: 30.0,
                y1: 10.0,
            },
            character_x: vec![0.0, 10.0, 20.0, 30.0],
            select_whole: false,
        };

        assert_eq!(target.character_at(16.0), 2);
        assert_eq!(target.selected_text(1, 2), "Ω");
        assert_eq!(target.selected_text(3, 1), "ΩB");
        assert_eq!(target.selection_rect(1, 3).x0, 10.0);
        assert_eq!(target.selection_rect(1, 3).x1, 30.0);
    }

    #[test]
    fn sheet_names_are_humanized_without_corrupting_part_numbers() {
        assert_eq!(display_sheet_name("01_primary_bms"), "Primary BMS");
        assert_eq!(
            display_sheet_name("02_secondary_protection"),
            "Secondary Protection"
        );
        assert_eq!(display_sheet_name("6s-bms-bq40z80"), "6S BMS BQ40Z80");
    }

    #[test]
    fn dark_palette_has_accessible_ui_and_schematic_contrast() {
        let palette = Theme::Dark.palette();
        for surface in [
            palette.app,
            palette.toolbar,
            palette.filmstrip,
            palette.card,
        ] {
            assert!(
                contrast_ratio(palette.text, surface) >= 7.0,
                "primary UI text must retain enhanced contrast"
            );
        }
        assert!(contrast_ratio(palette.card_border, palette.card) >= 3.0);

        for foreground in [
            palette.accent,
            palette.border,
            palette.bus,
            palette.junction,
            palette.label,
            palette.no_connect,
            palette.pin,
            palette.sheet_file,
            palette.symbol,
            palette.text,
            palette.wire,
            palette.selection,
        ] {
            assert!(
                contrast_ratio(foreground, palette.page) >= 4.5,
                "schematic foreground must remain legible on the dark page"
            );
        }
    }

    #[test]
    fn object_kinds_remain_editor_visible() {
        assert_ne!(
            crate::native_scene::ObjectKind::Symbol,
            crate::native_scene::ObjectKind::Wire
        );
    }

    #[test]
    fn latency_summary_uses_nearest_rank_p95() {
        let mut samples = (1..=20).map(f64::from).collect::<Vec<_>>();

        let summary = latency_summary(&mut samples);

        assert_eq!(summary.mean_ms, 10.5);
        assert_eq!(summary.p95_ms, 19.0);
        assert_eq!(summary.max_ms, 20.0);
    }

    #[test]
    fn next_sheet_instance_uses_root_uuid_and_next_numeric_page() {
        let source = r#"(kicad_sch
          (uuid "root-a")
          (sheet (at 1 1) (size 10 10) (uuid "sheet-a")
            (instances (project "demo" (path "/root-a" (page "4")))))
          (sheet (at 20 1) (size 10 10) (uuid "sheet-b")
            (instances (project "demo" (path "/root-a" (page "2"))))))"#;

        assert_eq!(
            next_sheet_instance(source).unwrap(),
            ("/root-a".to_owned(), "5".to_owned())
        );
    }

    #[test]
    fn sheet_pin_projection_uses_the_nearest_border_and_snaps_along_it() {
        let rectangle = (10.0, 20.0, 90.0, 70.0);

        let (right, right_rotation) =
            nearest_sheet_edge(rectangle, SchPoint { x: 88.0, y: 33.1 }, 2.54, true);
        let (top, top_rotation) =
            nearest_sheet_edge(rectangle, SchPoint { x: 44.2, y: 20.4 }, 2.54, true);

        assert_eq!(right.x, 90.0);
        assert!((right.y / 2.54 - (right.y / 2.54).round()).abs() < 1e-9);
        assert_eq!(right_rotation, 0.0);
        assert_eq!(top.y, 20.0);
        assert!((top.x / 2.54 - (top.x / 2.54).round()).abs() < 1e-9);
        assert_eq!(top_rotation, 90.0);
    }

    #[test]
    fn sheet_rectangle_reads_exact_at_and_size_geometry() {
        let source = r#"(kicad_sch
            (sheet (at 12.7 25.4) (size 80 50) (uuid "sheet-a")))"#;

        assert_eq!(
            sheet_rectangle(source, "sheet-a"),
            Some((12.7, 25.4, 92.7, 75.4))
        );
    }

    #[test]
    fn background_reload_loads_changed_but_not_known_unchanged_sheet() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("root.kicad_sch");
        std::fs::write(
            &path,
            r#"(kicad_sch (version 20250101) (uuid "root") (paper "A4"))"#,
        )
        .expect("write schematic");
        let key = path_key(&path);
        let generation = AtomicU64::new(1);
        let unchanged = ReloadRequest {
            generation: 1,
            root: path.clone(),
            changed: HashSet::new(),
            known: HashSet::from([key.clone()]),
            external: false,
        };
        let changed = ReloadRequest {
            generation: 1,
            root: path,
            changed: HashSet::from([key.clone()]),
            known: HashSet::from([key.clone()]),
            external: false,
        };

        let unchanged_batch =
            build_reload_batch(&unchanged, &generation).expect("current batch completes");
        let changed_batch =
            build_reload_batch(&changed, &generation).expect("current batch completes");

        assert!(unchanged_batch.loaded.is_empty());
        assert!(matches!(changed_batch.loaded.get(&key), Some(Ok(_))));
    }

    #[test]
    fn superseded_background_reload_is_cancelled() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("root.kicad_sch");
        std::fs::write(
            &path,
            r#"(kicad_sch (version 20250101) (uuid "root") (paper "A4"))"#,
        )
        .expect("write schematic");
        let request = ReloadRequest {
            generation: 1,
            root: path.clone(),
            changed: HashSet::from([path_key(&path)]),
            known: HashSet::new(),
            external: false,
        };
        let generation = AtomicU64::new(2);

        assert!(build_reload_batch(&request, &generation).is_none());
    }

    #[cfg(feature = "golden-svg-reference")]
    #[test]
    fn same_vello_oracle_flattens_kicad_style_groups() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10mm" height="10mm"
            viewBox="0 0 10 10"><g style="fill:#f5f4ef;stroke:#840000;stroke-width:0.1524">
            <rect x="0" y="0" width="10" height="10"/></g></svg>"##;
        let tree =
            vello_svg::usvg::Tree::from_str(svg, &vello_svg::usvg::Options::default()).unwrap();
        let mut scene = Scene::new();
        append_svg_group_flat(&mut scene, tree.root());
    }
}
