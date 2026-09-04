//! Which configuration file configured the running process.
//!
//! Konnect selects the **first existing** server configuration file from its
//! search list and loads only that one; later files are not merged in. Nothing
//! in the protocol reported which file won, so a user whose settings appeared to
//! be ignored had no way to tell a shadowed file from a malformed one (#419).
//!
//! This type records that decision once, at startup, and
//! `get_installation_info` reports it. It is deliberately captured rather than
//! recomputed on demand: re-running the search when the tool is called would
//! describe the filesystem as it is *now*, which may name a higher-priority file
//! created after launch — the opposite of the question being asked.
//!
//! The record carries paths only. Configuration values, file contents and IPC
//! credentials are never included.

use std::path::{Path, PathBuf};

/// How the running process obtained its configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// An explicit `--config <path>` was given; the search list was not consulted.
    ExplicitPath,
    /// A file from the automatic search list was selected.
    SearchPath,
    /// No candidate existed, so built-in defaults are in use.
    Defaults,
    /// The resolution was not recorded by this entry point. Reported rather than
    /// guessed, so a caller never reads a fabricated `defaults` for a load whose
    /// outcome is genuinely unknown.
    Unavailable,
}

impl ConfigSource {
    /// Stable public string, as reported in `get_installation_info`.
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigSource::ExplicitPath => "explicit_path",
            ConfigSource::SearchPath => "search_path",
            ConfigSource::Defaults => "defaults",
            ConfigSource::Unavailable => "unavailable",
        }
    }
}

/// The startup configuration decision, captured once and reported read-only.
#[derive(Debug, Clone)]
pub struct ConfigResolution {
    source: ConfigSource,
    selected_path: Option<PathBuf>,
    skipped_existing_paths: Vec<PathBuf>,
}

/// The only policy this build implements. Reported so a reader does not have to
/// infer from a single path whether later files were merged in.
pub const SEARCH_POLICY: &str = "first_existing_no_merge";

impl ConfigResolution {
    /// An explicit `--config <path>`. The automatic search list is not consulted,
    /// so nothing is reported as skipped: those files did not participate.
    pub fn explicit_path(path: impl AsRef<Path>) -> Self {
        Self {
            source: ConfigSource::ExplicitPath,
            selected_path: Some(absolutize(path.as_ref())),
            skipped_existing_paths: Vec::new(),
        }
    }

    /// A file chosen from the search list, plus any later candidates that exist
    /// and were therefore shadowed by it.
    pub fn search_path(selected: impl AsRef<Path>, skipped: &[PathBuf]) -> Self {
        Self {
            source: ConfigSource::SearchPath,
            selected_path: Some(absolutize(selected.as_ref())),
            skipped_existing_paths: skipped.iter().map(|p| absolutize(p)).collect(),
        }
    }

    /// No candidate existed. `selected_path` is `None`.
    pub fn defaults() -> Self {
        Self {
            source: ConfigSource::Defaults,
            selected_path: None,
            skipped_existing_paths: Vec::new(),
        }
    }

    /// Used by entry points that do not track the decision, so provenance is
    /// absent rather than wrong.
    pub fn unavailable() -> Self {
        Self {
            source: ConfigSource::Unavailable,
            selected_path: None,
            skipped_existing_paths: Vec::new(),
        }
    }

    pub fn source(&self) -> ConfigSource {
        self.source
    }

    pub fn selected_path(&self) -> Option<&Path> {
        self.selected_path.as_deref()
    }

    pub fn skipped_existing_paths(&self) -> &[PathBuf] {
        &self.skipped_existing_paths
    }
}

impl Default for ConfigResolution {
    fn default() -> Self {
        Self::unavailable()
    }
}

/// Absolute form for diagnosis, because a relative candidate such as
/// `konnect.toml` is meaningless to a reader who does not know the server's
/// working directory. `canonicalize` also resolves symlinks, which is what makes
/// the answer checkable; if it fails the path is returned unchanged rather than
/// dropped.
fn absolutize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|dir| dir.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_report_no_selected_path() {
        let resolution = ConfigResolution::defaults();
        assert_eq!(resolution.source().as_str(), "defaults");
        assert!(resolution.selected_path().is_none());
        assert!(resolution.skipped_existing_paths().is_empty());
    }

    #[test]
    fn unavailable_is_not_reported_as_defaults() {
        // The distinction #419's reviewer asked for: an entry point that did not
        // track the load must not look like a clean defaults selection.
        assert_eq!(
            ConfigResolution::unavailable().source().as_str(),
            "unavailable"
        );
        assert_ne!(
            ConfigResolution::unavailable().source(),
            ConfigResolution::defaults().source()
        );
    }

    #[test]
    fn explicit_path_reports_no_skipped_candidates() {
        // The automatic list is not consulted for --config, so reporting entries
        // as "skipped" would claim they took part in a search that never ran.
        let resolution = ConfigResolution::explicit_path("konnect.toml");
        assert_eq!(resolution.source().as_str(), "explicit_path");
        assert!(resolution.skipped_existing_paths().is_empty());
    }

    #[test]
    fn selected_and_skipped_paths_are_absolute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let selected = dir.path().join("konnect.toml");
        let skipped = dir.path().join("settings.json");
        std::fs::write(&selected, "").expect("write selected");
        std::fs::write(&skipped, "").expect("write skipped");

        let resolution = ConfigResolution::search_path(&selected, &[skipped]);
        assert!(resolution.selected_path().expect("selected").is_absolute());
        assert!(resolution
            .skipped_existing_paths()
            .iter()
            .all(|path| path.is_absolute()));
    }

    #[test]
    fn a_relative_path_is_made_absolute_even_when_it_does_not_exist() {
        let resolution = ConfigResolution::explicit_path("no_such_konnect.toml");
        assert!(resolution.selected_path().expect("selected").is_absolute());
    }
}
