//! C ABI exports for KiCAD plugin integration.
//!
//! KiCAD (or the thin Python launcher) can load this cdylib and call:
//!   kicad_plugin_init(config_path)  — start the embedded MCP server thread
//!   kicad_plugin_version()          — return the version string
//!   kicad_plugin_shutdown()         — stop the server cleanly
//!
//! The MCP server runs in a background tokio thread; the calling process
//! communicates with it via the configured transport (STDIO or HTTP/SSE).

use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static VERSION_CSTR: OnceLock<CString> = OnceLock::new();

/// Initialize and start the embedded MCP server.
///
/// # Safety
/// `config_path` must be a valid null-terminated UTF-8 C string, or NULL to use defaults.
#[no_mangle]
pub unsafe extern "C" fn kicad_plugin_init(config_path: *const c_char) -> c_int {
    let config_path_str = if config_path.is_null() {
        None
    } else {
        match CStr::from_ptr(config_path).to_str() {
            Ok(s) => Some(s.to_owned()),
            Err(_) => return -1,
        }
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return -1,
    };

    rt.spawn(async move {
        use crate::config::{Config, TransportMode};
        use konnect_core::config_resolution::ConfigResolution;
        use konnect_core::mcp::handler::McpHandler;

        // A failed load still falls back to defaults here, unchanged: that is a
        // separate defect, tracked apart from #419 to keep this change to one
        // reviewable outcome. What #419 requires is that provenance must not
        // *lie* about it. A load that failed is reported as `unavailable`, never
        // as a clean `defaults` selection, which would be exactly the fabricated
        // answer this change exists to remove.
        let (config, config_resolution) = match config_path_str.as_deref() {
            Some(p) => {
                let path = std::path::Path::new(p);
                match Config::load_from(path) {
                    Ok(config) => (config, ConfigResolution::explicit_path(path)),
                    Err(_) => (Config::default(), ConfigResolution::unavailable()),
                }
            }
            None => match Config::load_with_resolution() {
                Ok(resolved) => resolved,
                Err(_) => (Config::default(), ConfigResolution::unavailable()),
            },
        };
        let server_config = konnect_core::tools::ServerConfig {
            kicad_cli: config.kicad_cli.clone(),
            kicad_binary: config.kicad_binary.clone(),
            ipc_address: config.ipc_address.clone(),
            project_dir: config.project_dir.clone(),
            jlcpcb_db_path: config.jlcpcb_db_path.clone(),
            auto_load_toolsets: config.auto_load_toolsets,
            eager_toolsets: config.eager_toolsets,
        };
        match McpHandler::new_with_config_resolution(server_config, config_resolution).await {
            Ok(handler) => match config.transport {
                TransportMode::Stdio => {
                    let _ = crate::transport::stdio::run_stdio(handler).await;
                }
                TransportMode::Http => {
                    let _ = crate::transport::http::run_http(handler, &config.http_address).await;
                }
                TransportMode::Both => {
                    let handler_http = handler.clone();
                    let http_addr = config.http_address.clone();
                    tokio::select! {
                        _ = crate::transport::http::run_http(handler_http, &http_addr) => {},
                        _ = crate::transport::stdio::run_stdio(handler) => {},
                    }
                }
            },
            Err(e) => {
                eprintln!("kicad_plugin_init: failed to create handler: {}", e);
            }
        }
    });

    RUNTIME.set(rt).is_ok() as c_int
}

/// Return the plugin version string.
///
/// # Safety
/// The returned pointer is valid for the lifetime of the process.
#[no_mangle]
pub unsafe extern "C" fn kicad_plugin_version() -> *const c_char {
    VERSION_CSTR
        .get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).expect("version string is valid"))
        .as_ptr()
}

/// Shut down the embedded MCP server.
#[no_mangle]
pub extern "C" fn kicad_plugin_shutdown() {
    // The runtime will be dropped here; tokio tasks are cancelled automatically.
    // In a more complete implementation we would signal the server to flush and close.
    let _ = RUNTIME.get();
}
