use anyhow::Result;
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
        let source = match self {
            IpcAddressSource::Config => "config",
            IpcAddressSource::Environment => "KICAD_API_SOCKET",
            IpcAddressSource::Detected => "auto-detection",
            IpcAddressSource::Unresolved => {
                warn!(
                    "No KiCad IPC socket found (no KICAD_API_SOCKET, none detected at {}). \
                     Live-KiCad tools will fail and file-backed ones will edit the \
                     project on disk instead. Enable Edit > Preferences > Plugins > \
                     'Enable KiCad API' in KiCad, or set ipc_address in your config.",
                    konnect_ipc::candidate_socket_paths()
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return;
            }
        };
        info!("KiCad IPC address from {source}: {address}");
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
    /// `ipc_address` resolved either way.
    ///
    /// Every entry point loads through here. Resolution used to be the
    /// caller's job, and each caller that forgot it was a bug: #39 for
    /// `main.rs`, and `ffi.rs` silently ignoring KICAD_API_SOCKET until the
    /// same fix reached it.
    pub fn load_resolved(path: Option<&std::path::Path>) -> Result<(Self, IpcAddressSource)> {
        match path {
            Some(path) => {
                let mut config = Self::load_from(path)?;
                let ipc_source = config.resolve_ipc_address();
                Ok((config, ipc_source))
            }
            None => Self::load(),
        }
    }

    /// Load config from the default search path, with `ipc_address` resolved.
    pub fn load() -> Result<(Self, IpcAddressSource)> {
        let mut config_paths = vec![
            PathBuf::from("konnect.toml"),
            PathBuf::from("settings.json"),
        ];
        config_paths.extend(exe_relative_settings_paths());
        config_paths.push(dirs_config_path());

        let mut config = None;
        for path in &config_paths {
            if path.exists() {
                config = Some(Self::load_from(path)?);
                break;
            }
        }

        let mut config = config.unwrap_or_default();
        let ipc_source = config.resolve_ipc_address();
        Ok((config, ipc_source))
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
        if !self.ipc_address.is_empty() {
            return IpcAddressSource::Config;
        }
        if let Ok(sock) = std::env::var("KICAD_API_SOCKET") {
            if !sock.is_empty() {
                self.ipc_address = sock;
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
}
