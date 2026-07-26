//! Process-lifetime, navigable history for local and external schematic edits.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const INTRO_DURATION: Duration = Duration::from_millis(420);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeOrigin {
    Local,
    External,
    Undo,
    Redo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Add,
    Delete,
    Duplicate,
    Edit,
    External,
    Move,
    Redo,
    Transform,
    Undo,
    Wire,
}

impl ChangeKind {
    fn classify(origin: ChangeOrigin, label: &str) -> Self {
        if origin == ChangeOrigin::Undo {
            return Self::Undo;
        }
        if origin == ChangeOrigin::Redo {
            return Self::Redo;
        }
        let label = label.to_ascii_lowercase();
        if label.contains("duplicat") {
            Self::Duplicate
        } else if label.contains("delet") || label.contains("remov") {
            Self::Delete
        } else if label.contains("wire") || label.contains("bus") {
            Self::Wire
        } else if label.contains("mov") || label.contains("nudge") {
            Self::Move
        } else if label.contains("rotat") || label.contains("mirror") || label.contains("transform")
        {
            Self::Transform
        } else if label.contains("add")
            || label.contains("creat")
            || label.contains("insert")
            || label.contains("place")
        {
            Self::Add
        } else if label.contains("edit")
            || label.contains("chang")
            || label.contains("renam")
            || label.contains("resiz")
            || label.contains("propert")
        {
            Self::Edit
        } else if origin == ChangeOrigin::External {
            Self::External
        } else {
            Self::Edit
        }
    }
}

impl ChangeOrigin {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Local => "You",
            Self::External => "Konnect",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChangeEvent {
    pub(crate) id: u64,
    pub(crate) origin: ChangeOrigin,
    pub(crate) kind: ChangeKind,
    pub(crate) label: String,
    pub(crate) file: PathBuf,
    pub(crate) uuids: Vec<String>,
    pub(crate) recorded_at: SystemTime,
    introduced_at: Instant,
}

impl ChangeEvent {
    pub(crate) fn intro_progress(&self, now: Instant) -> f64 {
        (now.saturating_duration_since(self.introduced_at)
            .as_secs_f64()
            / INTRO_DURATION.as_secs_f64())
        .clamp(0.0, 1.0)
    }

    pub(crate) fn is_animating(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.introduced_at) < INTRO_DURATION
    }
}

#[derive(Debug, Default)]
pub(crate) struct ChangeTimeline {
    events: VecDeque<ChangeEvent>,
    next_id: u64,
}

impl ChangeTimeline {
    pub(crate) fn push(
        &mut self,
        origin: ChangeOrigin,
        label: impl Into<String>,
        file: PathBuf,
        mut uuids: Vec<String>,
    ) -> u64 {
        uuids.sort_unstable();
        uuids.dedup();
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let label = label.into();
        self.events.push_back(ChangeEvent {
            id: self.next_id,
            origin,
            kind: ChangeKind::classify(origin, &label),
            label,
            file,
            uuids,
            recorded_at: SystemTime::now(),
            introduced_at: Instant::now(),
        });
        self.next_id
    }

    pub(crate) fn events(&self) -> impl DoubleEndedIterator<Item = &ChangeEvent> {
        self.events.iter()
    }

    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    pub(crate) fn event(&self, id: u64) -> Option<&ChangeEvent> {
        self.events.iter().find(|event| event.id == id)
    }

    pub(crate) fn is_animating(&self, now: Instant) -> bool {
        self.events
            .back()
            .is_some_and(|event| event.is_animating(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_retains_complete_process_lifetime_history_and_deduplicates_item_ids() {
        let mut timeline = ChangeTimeline::default();
        for index in 0..=500 {
            timeline.push(
                ChangeOrigin::Local,
                format!("change {index}"),
                PathBuf::from("root.kicad_sch"),
                vec!["same".to_owned(), "same".to_owned()],
            );
        }
        assert_eq!(timeline.events().count(), 501);
        assert_eq!(timeline.events().next().unwrap().id, 1);
        assert_eq!(timeline.events().next_back().unwrap().id, 501);
        assert_eq!(timeline.events().next_back().unwrap().uuids, ["same"]);
    }

    #[test]
    fn origin_labels_are_human_readable() {
        assert_eq!(ChangeOrigin::Local.label(), "You");
        assert_eq!(ChangeOrigin::External.label(), "Konnect");
    }

    #[test]
    fn every_change_is_classified_for_iconography() {
        for (origin, label, expected) in [
            (ChangeOrigin::Local, "Moved symbol", ChangeKind::Move),
            (ChangeOrigin::Local, "Delete items", ChangeKind::Delete),
            (ChangeOrigin::Local, "Add wire", ChangeKind::Wire),
            (ChangeOrigin::Local, "Rotate symbol", ChangeKind::Transform),
            (
                ChangeOrigin::Local,
                "Duplicate symbol",
                ChangeKind::Duplicate,
            ),
            (ChangeOrigin::Local, "Edit Value", ChangeKind::Edit),
            (
                ChangeOrigin::External,
                "File reloaded",
                ChangeKind::External,
            ),
            (ChangeOrigin::Undo, "Moved symbol", ChangeKind::Undo),
            (ChangeOrigin::Redo, "Moved symbol", ChangeKind::Redo),
        ] {
            assert_eq!(ChangeKind::classify(origin, label), expected);
        }
    }
}
