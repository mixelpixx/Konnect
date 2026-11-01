use anyhow::Result;
use konnect_core::config_resolution::ConfigResolution;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the kicad-cli binary
    #[serde(default = "default_kicad_cli")]
    pub kicad_cli: String,

    /// Path to the KiCAD binary (for launching the UI)
    #[serde(default = "default_kicad_binary")]
    pub kicad_binary: String,

    /// Default project directory
    #[serde(default)]
    pub project_dir: Option<PathBuf>,

    /// KiCad IPC socket path (NNG). When empty, resolved at startup from the
    /// KICAD_API_SOCKET env var, then from the platform's default socket path.
    #[serde(default)]
    #[serde(alias = "ipc_socket_path")]
    pub ipc_address: String,

    /// MCP server transport mode
    #[serde(default)]
    pub transport: TransportMode,

    /// HTTP server bind address (used when transport includes HTTP)
    #[serde(default = "default_http_address")]
    pub http_address: String,

    /// JLCPCB database cache path
    #[serde(default)]
    pub jlcpcb_db_path: Option<PathBuf>,

    /// Log level (error, warn, info, debug, trace)
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Auto-load a tool's toolset on call instead of returning
    /// `toolset_not_loaded`. Off by default: toolsets accumulate monotonically
    /// once loaded, so auto-load trades one recoverable error for permanent
    /// context growth -- opt in only if that trade is worth it for your client.
    #[serde(default)]
    pub auto_load_toolsets: bool,

    /// Pre-load every toolset at startup so the very first `tools/list` is
    /// complete. Off by default: a full listing costs roughly 25K tokens
    /// against the ~2K baseline, which is the whole reason the router exists.
    ///
    /// Turn it on for an MCP client that caches the initial tool list and does
    /// not act on `notifications/tools/list_changed`. For those clients a tool
    /// missing from the first listing can never be called at all --
    /// `load_toolset` reports the names it loaded but returns no schemas, so
    /// there is nothing for the client to invoke, and `auto_load_toolsets`
    /// cannot help because it only fires once a call is actually attempted.
    #[serde(default)]
    pub eager_toolsets: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    #[default]
    Stdio,
    Http,
    Both,
}

/// Where the effective `ipc_address` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcAddressSource {
    /// Set explicitly in a config file.
    Config,
    /// Taken from `KICAD_API_SOCKET` (set for plugins KiCad launches itself).
    Environment,
    /// Probed from the platform's default KiCad socket path.
    Detected,
    /// Nothing found — IPC tools will report the socket as unconfigured, and
    /// the ones with a file path will quietly use it.
    Unresolved,
}

impl IpcAddressSource {
    /// Report the resolution once tracing is initialized.
    pub fn log(self, address: &str) {
        if let Some(message) = self.resolved_log_message(address) {
            info!("{message}");
            return;
        }

        warn!(
            "No KiCad IPC socket found (no KICAD_API_SOCKET, none detected at {}). \
             Live-KiCad tools will fail and file-backed ones will edit the \
             project on disk instead. Enable Preferences > Plugins > \
             'Enable KiCad API' (KiCad > Preferences on macOS), or set ipc_address in your config.",
            konnect_ipc::candidate_socket_paths()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    fn resolved_log_message(self, address: &str) -> Option<String> {
        let source = match self {
            IpcAddressSource::Config => "config",
            IpcAddressSource::Environment => "KICAD_API_SOCKET",
            IpcAddressSource::Detected => "auto-detection",
            IpcAddressSource::Unresolved => return None,
        };
        Some(format!(
            "KiCad IPC address from {source}: {}",
            konnect_ipc::redact_endpoint(address)
        ))
    }
}

fn default_kicad_cli() -> String {
    if cfg!(target_os = "windows") {
        "kicad-cli.exe".to_string()
    } else {
        "kicad-cli".to_string()
    }
}

fn default_kicad_binary() -> String {
    if cfg!(target_os = "windows") {
        "kicad.exe".to_string()
    } else {
        "kicad".to_string()
    }
}

fn default_http_address() -> String {
    "127.0.0.1:3000".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Config {
    /// Load from `path` when given, else from the default search path, with
    /// both the effective `ipc_address` source and configuration-file
    /// provenance resolved either way.
    ///
    /// Every entry point loads through here. Resolution used to be the
    /// caller's job, and each caller that forgot it was a bug: #39 for
    /// `main.rs`, and `ffi.rs` silently ignoring KICAD_API_SOCKET until the
    /// same fix reached it.
    pub fn load_resolved(
        path: Option<&std::path::Path>,
    ) -> Result<(Self, IpcAddressSource, ConfigResolution)> {
        match path {
            Some(path) => {
                let mut config = Self::load_from(path)?;
                let ipc_source = config.resolve_ipc_address();
                Ok((config, ipc_source, ConfigResolution::explicit_path(path)))
            }
            None => Self::load(),
        }
    }

    /// Load config from the default search path, reporting which file was
    /// selected and which later existing files it shadowed, so
    /// `get_installation_info` can say what configured the process instead of
    /// leaving the user to guess (#419).
    ///
    /// Behaviour is unchanged: the first existing candidate wins, a malformed
    /// selected file is an error rather than permission to fall through to a
    /// later one, no candidates means defaults, and the environment fallback is
    /// applied after file or default resolution.
    fn load() -> Result<(Self, IpcAddressSource, ConfigResolution)> {
        let (selected, skipped) = select_config_candidate(&default_config_paths());

        let (mut config, resolution) = match selected {
            Some(path) => {
                let config = Self::load_from(&path)?;
                (config, ConfigResolution::search_path(&path, &skipped))
            }
            None => (Self::default(), ConfigResolution::defaults()),
        };

        let ipc_source = config.resolve_ipc_address();
        Ok((config, ipc_source, resolution))
    }

    /// Fill in a blank `ipc_address`: env var first, then the platform default
    /// KiCad listens on. Must run on every load path — including
    /// `--config <file>`, which is how KiCad itself launches the server (with
    /// KICAD_API_SOCKET in the environment).
    ///
    /// Returns where the address came from so the caller can log it once
    /// tracing is up; a session that will silently fall back to file editing
    /// says so at startup rather than at the first confusing tool result.
    pub fn resolve_ipc_address(&mut self) -> IpcAddressSource {
        self.resolve_ipc_address_with(konnect_ipc::detect_ipc_address)
    }

    fn resolve_ipc_address_with(
        &mut self,
        detect_ipc_address: impl FnOnce() -> Option<String>,
    ) -> IpcAddressSource {
        let trimmed = self.ipc_address.trim().to_string();
        if !trimmed.is_empty() {
            self.ipc_address = trimmed;
            return IpcAddressSource::Config;
        }
        self.ipc_address.clear();
        if let Ok(sock) = std::env::var("KICAD_API_SOCKET") {
            let sock = sock.trim();
            if !sock.is_empty() {
                self.ipc_address = sock.to_string();
                return IpcAddressSource::Environment;
            }
        }
        match detect_ipc_address() {
            Some(address) => {
                self.ipc_address = address;
                IpcAddressSource::Detected
            }
            None => IpcAddressSource::Unresolved,
        }
    }

    /// Load config from a specific file path. Auto-detects JSON vs TOML by extension.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        match ext {
            "json" => {
                let config: Config = serde_json::from_str(&content)?;
                Ok(config)
            }
            _ => {
                // Default: TOML
                let config: Config = toml::from_str(&content)?;
                Ok(config)
            }
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            kicad_cli: default_kicad_cli(),
            kicad_binary: default_kicad_binary(),
            project_dir: None,
            // Blank until `resolve_ipc_address` fills it in.
            ipc_address: String::new(),
            transport: TransportMode::default(),
            http_address: default_http_address(),
            jlcpcb_db_path: None,
            log_level: default_log_level(),
            auto_load_toolsets: false,
            eager_toolsets: false,
        }
    }
}

/// The configuration search list, in precedence order. Only the first existing
/// entry is loaded; the rest are shadowed, never merged.
fn default_config_paths() -> Vec<PathBuf> {
    let mut config_paths = vec![
        PathBuf::from("konnect.toml"),
        PathBuf::from("settings.json"),
    ];
    config_paths.extend(exe_relative_settings_paths());
    config_paths.push(dirs_config_path());
    config_paths
}

/// Pick the first existing candidate, and report the later ones that exist and
/// are therefore shadowed by it.
///
/// Split out from `Config::load` so precedence is testable against a supplied
/// list: the real list depends on the process working directory and
/// `current_exe()`, neither of which a test can change without affecting the
/// whole process.
fn select_config_candidate(candidates: &[PathBuf]) -> (Option<PathBuf>, Vec<PathBuf>) {
    let mut selected: Option<PathBuf> = None;
    let mut skipped = Vec::new();

    for path in candidates {
        if !path.exists() {
            continue;
        }
        if selected.is_none() {
            selected = Some(path.clone());
        } else {
            skipped.push(path.clone());
        }
    }

    (selected, skipped)
}

/// settings.json next to the binary, and one dir up (covers <plugin_dir>/bin/konnect).
fn exe_relative_settings_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            paths.push(exe_dir.join("settings.json"));
            if let Some(parent_dir) = exe_dir.parent() {
                paths.push(parent_dir.join("settings.json"));
            }
        }
    }
    paths
}

fn dirs_config_path() -> PathBuf {
    // Platform-specific config directory
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("konnect").join("config.toml")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("konnect")
            .join("config.toml")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home)
            .join(".config")
            .join("konnect")
            .join("config.toml")
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(ext: &str, content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    // Malformed input must produce Err, never a panic (the class of bug
    // PR #9 found in the config *tools*; this pins the server config too).

    #[test]
    fn json_non_object_root_is_err_not_panic() {
        for bad in ["[1, 2, 3]", "42", "\"just a string\"", "null", "true"] {
            let f = write_temp("json", bad);
            assert!(Config::load_from(f.path()).is_err(), "input: {bad}");
        }
    }

    #[test]
    fn json_wrong_field_types_are_err() {
        for bad in [
            r#"{"transport": 42}"#,
            r#"{"transport": "carrier-pigeon"}"#,
            r#"{"kicad_cli": ["a", "b"]}"#,
            r#"{"log_level": {"nested": true}}"#,
        ] {
            let f = write_temp("json", bad);
            assert!(Config::load_from(f.path()).is_err(), "input: {bad}");
        }
    }

    #[test]
    fn toml_garbage_is_err_not_panic() {
        for bad in ["= = =", "[unclosed", "transport = ", "\u{0000}\u{FFFF}"] {
            let f = write_temp("toml", bad);
            assert!(Config::load_from(f.path()).is_err(), "input: {bad:?}");
        }
    }

    #[test]
    fn missing_file_is_err() {
        assert!(Config::load_from(std::path::Path::new("does/not/exist.toml")).is_err());
    }

    // Partial configs fill in defaults for everything omitted.

    #[test]
    fn empty_json_object_yields_defaults() {
        let f = write_temp("json", "{}");
        let c = Config::load_from(f.path()).unwrap();
        let d = Config::default();
        assert_eq!(c.kicad_cli, d.kicad_cli);
        assert_eq!(c.http_address, d.http_address);
        assert_eq!(c.log_level, d.log_level);
        assert!(matches!(c.transport, TransportMode::Stdio));
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let f = write_temp("toml", "");
        let c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.log_level, "info");
    }

    #[test]
    fn partial_toml_overrides_only_named_fields() {
        let f = write_temp(
            "toml",
            "transport = \"http\"\nhttp_address = \"127.0.0.1:9999\"\n",
        );
        let c = Config::load_from(f.path()).unwrap();
        assert!(matches!(
            c.transport,
            TransportMode::Both | TransportMode::Http
        ));
        assert!(matches!(c.transport, TransportMode::Http));
        assert_eq!(c.http_address, "127.0.0.1:9999");
        assert_eq!(c.log_level, "info"); // untouched default
    }

    // Mutates the process-wide env var, so these two run serially.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn empty_ipc_address_falls_back_to_env_var_when_no_config_found() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("KICAD_API_SOCKET", "ipc://env-fallback.sock");
        let mut c = Config::default();
        assert_eq!(c.resolve_ipc_address(), IpcAddressSource::Environment);
        assert_eq!(c.ipc_address, "ipc://env-fallback.sock");
        std::env::remove_var("KICAD_API_SOCKET");
    }

    #[test]
    fn blank_env_value_falls_back_to_detection() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("KICAD_API_SOCKET", " \t ");

        let mut c = Config::default();
        let source = c.resolve_ipc_address_with(|| Some("ipc:///tmp/kicad/api.sock".to_string()));

        assert_eq!(source, IpcAddressSource::Detected);
        assert_eq!(c.ipc_address, "ipc:///tmp/kicad/api.sock");
        std::env::remove_var("KICAD_API_SOCKET");
    }

    #[test]
    fn resolved_log_message_redacts_endpoint_secrets() {
        let message = IpcAddressSource::Environment
            .resolved_log_message("tcp://user:secret@127.0.0.1:9000?token=hidden#detail")
            .unwrap();

        assert_eq!(
            message,
            "KiCad IPC address from KICAD_API_SOCKET: \
             tcp://[redacted]@127.0.0.1:9000 [query/fragment redacted]"
        );
        assert!(!message.contains("secret"));
        assert!(!message.contains("hidden"));
    }

    #[test]
    fn explicit_empty_ipc_address_in_config_file_does_not_block_env_var() {
        // A present-but-blank field must not out-rank the env var the way
        // a merely-missing field would (#39).
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("KICAD_API_SOCKET", "ipc://env-wins.sock");

        let f = write_temp("json", r#"{"ipc_socket_path": ""}"#);
        let mut c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.ipc_address, "", "sanity: file's blank value loaded as-is");

        c.resolve_ipc_address();
        assert_eq!(c.ipc_address, "ipc://env-wins.sock");

        // But an explicit file value must out-rank the env var.
        let f = write_temp("json", r#"{"ipc_socket_path": "ipc://file-wins.sock"}"#);
        let mut c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.resolve_ipc_address(), IpcAddressSource::Config);
        assert_eq!(c.ipc_address, "ipc://file-wins.sock");

        std::env::remove_var("KICAD_API_SOCKET");
    }

    #[test]
    fn whitespace_only_ipc_address_in_config_file_does_not_block_env_or_detection() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::remove_var("KICAD_API_SOCKET");

        let f = write_temp("json", r#"{"ipc_socket_path": "   "}"#);
        let mut c = Config::load_from(f.path()).unwrap();
        let source = c.resolve_ipc_address_with(|| Some("ipc:///tmp/kicad/api.sock".to_string()));
        assert_eq!(source, IpcAddressSource::Detected);
        assert_eq!(c.ipc_address, "ipc:///tmp/kicad/api.sock");
    }

    // Auto-detection: only reached when neither the file nor the env var
    // names an address.

    #[test]
    fn blank_address_and_no_env_var_falls_back_to_the_detected_socket() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::remove_var("KICAD_API_SOCKET");

        let mut c = Config::default();
        let source = c.resolve_ipc_address_with(|| Some("ipc:///tmp/kicad/api.sock".to_string()));
        assert_eq!(source, IpcAddressSource::Detected);
        assert_eq!(c.ipc_address, "ipc:///tmp/kicad/api.sock");
    }

    #[test]
    fn env_var_out_ranks_the_detected_socket() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("KICAD_API_SOCKET", "ipc://env-wins.sock");

        let mut c = Config::default();
        let source = c.resolve_ipc_address_with(|| Some("ipc:///tmp/kicad/api.sock".to_string()));
        assert_eq!(source, IpcAddressSource::Environment);
        assert_eq!(c.ipc_address, "ipc://env-wins.sock");

        std::env::remove_var("KICAD_API_SOCKET");
    }

    #[test]
    fn config_value_out_ranks_the_detected_socket_and_is_not_probed() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::remove_var("KICAD_API_SOCKET");

        let f = write_temp("json", r#"{"ipc_socket_path": "ipc://file-wins.sock"}"#);
        let mut c = Config::load_from(f.path()).unwrap();
        let source = c.resolve_ipc_address_with(|| panic!("must not probe a configured address"));
        assert_eq!(source, IpcAddressSource::Config);
        assert_eq!(c.ipc_address, "ipc://file-wins.sock");
    }

    #[test]
    fn nothing_found_leaves_the_address_empty() {
        // Empty keeps the "socket path not configured" guidance in the tools'
        // errors instead of a dial failure against a guessed address.
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::remove_var("KICAD_API_SOCKET");

        let mut c = Config::default();
        let source = c.resolve_ipc_address_with(|| None);
        assert_eq!(source, IpcAddressSource::Unresolved);
        assert_eq!(c.ipc_address, "");
    }

    #[test]
    fn legacy_ipc_socket_path_alias_still_works() {
        // settings.json written by the KiCAD plugin dialog uses the alias.
        let f = write_temp("json", r#"{"ipc_socket_path": "ipc://test.sock"}"#);
        let c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.ipc_address, "ipc://test.sock");
    }

    #[test]
    fn unknown_extension_parses_as_toml() {
        let f = write_temp("conf", "log_level = \"debug\"\n");
        let c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.log_level, "debug");
    }

    // ─── Config provenance (#419) ─────────────────────────────────────────
    //
    // The candidate list is supplied rather than discovered so precedence is
    // testable: the real list depends on the working directory and
    // `current_exe()`, and changing either is process-wide.

    fn touch(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "").expect("write candidate");
        path
    }

    #[test]
    fn no_candidate_exists_selects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = vec![dir.path().join("konnect.toml"), dir.path().join("a.json")];

        let (selected, skipped) = select_config_candidate(&candidates);

        assert!(selected.is_none(), "nothing exists, so nothing is selected");
        assert!(skipped.is_empty());
    }

    #[test]
    fn first_missing_second_existing_selects_the_second() {
        let dir = tempfile::tempdir().unwrap();
        let second = touch(dir.path(), "settings.json");
        let candidates = vec![dir.path().join("konnect.toml"), second.clone()];

        let (selected, skipped) = select_config_candidate(&candidates);

        assert_eq!(selected.as_ref(), Some(&second));
        assert!(skipped.is_empty(), "nothing exists after the selected file");
    }

    #[test]
    fn first_wins_and_a_later_existing_file_is_reported_as_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let first = touch(dir.path(), "konnect.toml");
        let missing = dir.path().join("settings.json");
        let third = touch(dir.path(), "config.toml");
        let candidates = vec![first.clone(), missing, third.clone()];

        let (selected, skipped) = select_config_candidate(&candidates);

        assert_eq!(selected.as_ref(), Some(&first), "first existing wins");
        assert_eq!(
            skipped,
            vec![third],
            "the shadowed file is named, not merged"
        );
    }

    #[test]
    fn a_malformed_selected_file_is_an_error_not_a_fall_through() {
        // The behaviour most worth pinning: a broken file must not silently hand
        // over to a later valid one, which would load settings the user never
        // pointed at while their real file sat unreported.
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("konnect.toml");
        std::fs::write(&broken, "this is not = valid toml [[[").unwrap();
        let valid = dir.path().join("settings.json");
        std::fs::write(&valid, "{}").unwrap();

        let (selected, _) = select_config_candidate(&[broken.clone(), valid]);
        assert_eq!(selected.as_ref(), Some(&broken));
        assert!(
            Config::load_from(&broken).is_err(),
            "the selected file is malformed, so loading it must fail"
        );
    }

    #[test]
    fn defaults_resolution_reports_no_path() {
        let resolution = ConfigResolution::defaults();
        assert_eq!(resolution.source().as_str(), "defaults");
        assert!(resolution.selected_path().is_none());
    }

    #[test]
    fn an_explicit_config_does_not_report_the_automatic_list_as_skipped() {
        // --config bypasses discovery, so reporting search candidates as
        // "skipped" would claim they took part in a search that never ran.
        let dir = tempfile::tempdir().unwrap();
        let explicit = touch(dir.path(), "explicit.toml");

        let resolution = ConfigResolution::explicit_path(&explicit);

        assert_eq!(resolution.source().as_str(), "explicit_path");
        assert!(resolution.skipped_existing_paths().is_empty());
    }
}
