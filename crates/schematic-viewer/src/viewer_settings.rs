//! Persistent native-viewer preferences with conservative, cross-platform defaults.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct ViewerSettings {
    pub(crate) ui_scale: f32,
    pub(crate) highlight_changes: bool,
    pub(crate) follow_changes: bool,
    pub(crate) dark_theme: bool,
    pub(crate) diagnostics_panel_x: Option<f32>,
    pub(crate) diagnostics_panel_y: Option<f32>,
    pub(crate) diagnostics_collapsed_x: Option<f32>,
    pub(crate) diagnostics_collapsed_y: Option<f32>,
    pub(crate) diagnostics_collapsed: bool,
}

impl Default for ViewerSettings {
    fn default() -> Self {
        Self {
            ui_scale: 1.25,
            highlight_changes: true,
            follow_changes: false,
            dark_theme: false,
            diagnostics_panel_x: None,
            diagnostics_panel_y: None,
            diagnostics_collapsed_x: None,
            diagnostics_collapsed_y: None,
            diagnostics_collapsed: false,
        }
    }
}

impl ViewerSettings {
    pub(crate) fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> Self {
        let Ok(source) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&source)
            .map(Self::normalized)
            .unwrap_or_default()
    }

    pub(crate) fn save(self) -> Result<()> {
        let path = settings_path().context("no user configuration directory is available")?;
        self.save_to(&path)
    }

    fn save_to(self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("viewer settings path has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let source = serde_json::to_string_pretty(&self.normalized())?;
        konnect_sexp::write_atomic(path, &format!("{source}\n"))
            .with_context(|| format!("failed to save {}", path.display()))
    }

    pub(crate) fn cycle_ui_scale(&mut self) {
        const SCALES: &[f32] = &[1.0, 1.15, 1.25, 1.4, 1.6];
        let index = SCALES
            .iter()
            .position(|scale| (*scale - self.ui_scale).abs() < 0.01)
            .unwrap_or(2);
        self.ui_scale = SCALES[(index + 1) % SCALES.len()];
    }

    fn normalized(mut self) -> Self {
        if !self.ui_scale.is_finite() {
            self.ui_scale = Self::default().ui_scale;
        }
        self.ui_scale = self.ui_scale.clamp(1.0, 1.6);
        self.diagnostics_panel_x = self
            .diagnostics_panel_x
            .filter(|position| position.is_finite())
            .map(|position| position.clamp(0.0, 1.0));
        self.diagnostics_panel_y = self
            .diagnostics_panel_y
            .filter(|position| position.is_finite())
            .map(|position| position.clamp(0.0, 1.0));
        self.diagnostics_collapsed_x = self
            .diagnostics_collapsed_x
            .filter(|position| position.is_finite())
            .map(|position| position.clamp(0.0, 1.0));
        self.diagnostics_collapsed_y = self
            .diagnostics_collapsed_y
            .filter(|position| position.is_finite())
            .map(|position| position.clamp(0.0, 1.0));
        self
    }
}

fn settings_path() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("Konnect").join("viewer.json"));
    }
    if cfg!(target_os = "macos") {
        return std::env::var_os("HOME").map(PathBuf::from).map(|root| {
            root.join("Library")
                .join("Application Support")
                .join("Konnect")
                .join("viewer.json")
        });
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|root| Path::new(&root).join(".config")))
        .map(|root| root.join("konnect").join("viewer.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_clamps_ui_scale() {
        assert_eq!(
            ViewerSettings {
                ui_scale: 9.0,
                ..ViewerSettings::default()
            }
            .normalized()
            .ui_scale,
            1.6
        );
    }

    #[test]
    fn ui_scale_cycle_wraps() {
        let mut settings = ViewerSettings {
            ui_scale: 1.6,
            ..ViewerSettings::default()
        };
        settings.cycle_ui_scale();
        assert_eq!(settings.ui_scale, 1.0);
    }

    #[test]
    fn settings_round_trip_all_persistent_preferences() {
        let directory = tempfile::tempdir().expect("temporary settings directory");
        let path = directory.path().join("viewer.json");
        let expected = ViewerSettings {
            ui_scale: 1.4,
            highlight_changes: false,
            follow_changes: true,
            dark_theme: true,
            diagnostics_panel_x: Some(0.72),
            diagnostics_panel_y: Some(0.35),
            diagnostics_collapsed_x: Some(0.91),
            diagnostics_collapsed_y: Some(0.08),
            diagnostics_collapsed: true,
        };

        expected.save_to(&path).expect("save viewer settings");

        assert_eq!(ViewerSettings::load_from(&path), expected);
        assert!(std::fs::read_to_string(path)
            .expect("read viewer settings")
            .ends_with('\n'));
    }

    #[test]
    fn older_settings_without_theme_keep_the_light_default() {
        let directory = tempfile::tempdir().expect("temporary settings directory");
        let path = directory.path().join("viewer.json");
        std::fs::write(
            &path,
            r#"{"ui_scale":1.15,"highlight_changes":false,"follow_changes":true}"#,
        )
        .expect("write legacy settings");

        let loaded = ViewerSettings::load_from(&path);

        assert_eq!(loaded.ui_scale, 1.15);
        assert!(!loaded.highlight_changes);
        assert!(loaded.follow_changes);
        assert!(!loaded.dark_theme);
        assert_eq!(loaded.diagnostics_panel_x, None);
        assert_eq!(loaded.diagnostics_panel_y, None);
        assert_eq!(loaded.diagnostics_collapsed_x, None);
        assert_eq!(loaded.diagnostics_collapsed_y, None);
        assert!(!loaded.diagnostics_collapsed);
    }

    #[test]
    fn diagnostic_panel_positions_are_normalized() {
        let settings = ViewerSettings {
            diagnostics_panel_x: Some(1.4),
            diagnostics_panel_y: Some(-0.2),
            diagnostics_collapsed_x: Some(-0.4),
            diagnostics_collapsed_y: Some(2.0),
            ..ViewerSettings::default()
        }
        .normalized();

        assert_eq!(settings.diagnostics_panel_x, Some(1.0));
        assert_eq!(settings.diagnostics_panel_y, Some(0.0));
        assert_eq!(settings.diagnostics_collapsed_x, Some(0.0));
        assert_eq!(settings.diagnostics_collapsed_y, Some(1.0));
    }
}
