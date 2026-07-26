//! Pure editor policies shared by native input, reload, and transaction UI.

use crate::native_scene::{Bounds, Point};
use konnect_sexp::{
    parse_sexp, read_consistent, writer::find_direct_child_blocks, DocumentRevision,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use winit::dpi::PhysicalPosition;

pub(crate) fn snap_point(point: Point, enabled: bool, grid_mm: f64) -> Point {
    if !enabled {
        return point;
    }
    Point {
        x: (point.x / grid_mm).round() * grid_mm,
        y: (point.y / grid_mm).round() * grid_mm,
    }
}

pub(crate) fn drag_delta_mm(
    start: PhysicalPosition<f64>,
    current: PhysicalPosition<f64>,
    scale: f64,
    snap: bool,
    grid_mm: f64,
) -> (f64, f64) {
    let mut dx = (current.x - start.x) / scale.max(0.001);
    let mut dy = (current.y - start.y) / scale.max(0.001);
    if snap {
        dx = (dx / grid_mm).round() * grid_mm;
        dy = (dy / grid_mm).round() * grid_mm;
    }
    (dx, dy)
}

pub(crate) fn box_selects_bounds(bounds: Bounds, start: Point, end: Point) -> bool {
    let min_x = start.x.min(end.x);
    let max_x = start.x.max(end.x);
    let min_y = start.y.min(end.y);
    let max_y = start.y.max(end.y);
    if end.x < start.x {
        bounds.min_x <= max_x
            && bounds.max_x >= min_x
            && bounds.min_y <= max_y
            && bounds.max_y >= min_y
    } else {
        bounds.min_x >= min_x
            && bounds.max_x <= max_x
            && bounds.min_y >= min_y
            && bounds.max_y <= max_y
    }
}

pub(crate) fn kicad_lock_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("schematic.kicad_sch");
    parent.join(format!("~{filename}.lck"))
}

pub(crate) fn notification_matches_revision(path: &Path, expected: DocumentRevision) -> bool {
    read_consistent(path)
        .map(|source| DocumentRevision::of(&source) == expected)
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalChangeSummary {
    pub(crate) description: String,
    pub(crate) changed_uuids: Vec<String>,
}

pub(crate) fn summarize_external_change(
    name: &str,
    before: &str,
    after: &str,
) -> Option<ExternalChangeSummary> {
    let before_items = uuid_item_blocks(before);
    let after_items = uuid_item_blocks(after);
    let mut added = after_items
        .keys()
        .filter(|id| !before_items.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    let mut removed = before_items
        .keys()
        .filter(|id| !after_items.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    let mut modified = before_items
        .iter()
        .filter_map(|(id, block)| {
            after_items
                .get(id)
                .filter(|after_block| *after_block != block)
                .map(|_| id.clone())
        })
        .collect::<Vec<_>>();
    added.sort_unstable();
    removed.sort_unstable();
    modified.sort_unstable();
    if added.is_empty() && removed.is_empty() && modified.is_empty() {
        return None;
    }
    let examples = modified
        .iter()
        .take(3)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let detail = if examples.is_empty() {
        String::new()
    } else {
        format!(" · changed: {examples}")
    };
    let description = format!(
        "{name} · +{} added · −{} removed · ~{} modified{detail}",
        added.len(),
        removed.len(),
        modified.len()
    );
    let mut changed_uuids = Vec::with_capacity(added.len() + removed.len() + modified.len());
    changed_uuids.extend(added);
    changed_uuids.extend(removed);
    changed_uuids.extend(modified);
    Some(ExternalChangeSummary {
        description,
        changed_uuids,
    })
}

fn uuid_item_blocks(source: &str) -> HashMap<String, String> {
    let Ok(root) = parse_sexp(source) else {
        return HashMap::new();
    };
    let Some(head) = root.head() else {
        return HashMap::new();
    };
    find_direct_child_blocks(source, head)
        .into_iter()
        .filter_map(|(start, end)| {
            let block = &source[start..end];
            let node = parse_sexp(block).ok()?;
            let uuid = node.find("uuid")?.get(1)?.as_str()?.to_owned();
            Some((uuid, block.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_to_right_box_requires_full_containment() {
        let bounds = Bounds {
            min_x: 5.0,
            min_y: 5.0,
            max_x: 15.0,
            max_y: 15.0,
        };
        assert!(!box_selects_bounds(
            bounds,
            Point { x: 0.0, y: 0.0 },
            Point { x: 10.0, y: 10.0 }
        ));
        assert!(box_selects_bounds(
            bounds,
            Point { x: 0.0, y: 0.0 },
            Point { x: 20.0, y: 20.0 }
        ));
    }

    #[test]
    fn right_to_left_box_selects_crossing_geometry() {
        let bounds = Bounds {
            min_x: 5.0,
            min_y: 5.0,
            max_x: 15.0,
            max_y: 15.0,
        };
        assert!(box_selects_bounds(
            bounds,
            Point { x: 10.0, y: 10.0 },
            Point { x: 0.0, y: 0.0 }
        ));
    }

    #[test]
    fn drag_delta_snaps_once_at_commit_grid() {
        let start = PhysicalPosition::new(100.0, 100.0);
        let current = PhysicalPosition::new(112.0, 94.0);
        assert_eq!(drag_delta_mm(start, current, 10.0, true, 1.27), (1.27, 0.0));
        assert_eq!(
            drag_delta_mm(start, current, 10.0, false, 1.27),
            (1.2, -0.6)
        );
    }

    #[test]
    fn wire_points_use_the_active_grid() {
        let point = Point { x: 2.0, y: 3.0 };
        assert_eq!(snap_point(point, true, 1.27), Point { x: 2.54, y: 2.54 });
        assert_eq!(snap_point(point, false, 1.27), point);
    }

    #[test]
    fn external_change_summary_counts_uuid_owned_item_transitions() {
        let before = r#"(kicad_sch
            (wire (pts (xy 0 0) (xy 1 0)) (uuid "wire-a"))
            (junction (at 1 0) (uuid "junction-b")))"#;
        let after = r#"(kicad_sch
            (wire (pts (xy 0 0) (xy 2 0)) (uuid "wire-a"))
            (label "NET" (at 2 0 0) (uuid "label-c")))"#;
        let summary = summarize_external_change("Power", before, after).unwrap();
        assert!(summary.description.contains("+1 added"));
        assert!(summary.description.contains("−1 removed"));
        assert!(summary.description.contains("~1 modified"));
        assert!(summary.description.contains("wire-a"));
        assert_eq!(summary.changed_uuids, ["label-c", "junction-b", "wire-a"]);
    }

    #[test]
    fn local_notification_requires_the_exact_committed_revision() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "(kicad_sch (version 1))").unwrap();
        let revision = DocumentRevision::of("(kicad_sch (version 1))");
        assert!(notification_matches_revision(file.path(), revision));
        std::fs::write(file.path(), "(kicad_sch (version 2))").unwrap();
        assert!(!notification_matches_revision(file.path(), revision));
    }

    #[test]
    fn lock_path_uses_the_kicad_editor_convention() {
        assert_eq!(
            kicad_lock_path(Path::new("project/root.kicad_sch")),
            PathBuf::from("project/~root.kicad_sch.lck")
        );
    }
}
