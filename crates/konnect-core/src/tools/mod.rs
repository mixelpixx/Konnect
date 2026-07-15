//! Tool trait definitions, ToolContext, and all toolset modules.

pub mod cli;
pub mod config;
pub mod design_review;
pub mod integration;
pub mod library;
pub mod manufacturing;
pub mod pcb_board;
pub mod pcb_components;
pub mod pcb_export;
pub mod pcb_routing;
pub mod project;
pub mod sch_analysis;
pub mod sch_batch;
pub mod sch_bridge;
pub mod sch_components;
pub mod sch_export;
pub mod sch_hierarchy;
pub mod sch_wiring;
pub mod schematic_builder;
pub mod svg_import;
pub mod templates;
pub mod verification;

use crate::mcp::protocol::{CallToolResult, McpToolDescription};
use crate::router::ToolRouter;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ─── Tool Handler Type ────────────────────────────────────────────────────────

pub type ToolHandlerFn = Arc<
    dyn Fn(
            &Value,
            Arc<ToolContext>,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<CallToolResult>> + Send>>
        + Send
        + Sync,
>;

// ─── ToolDef ─────────────────────────────────────────────────────────────────

/// A single tool definition: schema + async handler.
#[derive(Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub handler: ToolHandlerFn,
}

impl ToolDef {
    pub fn to_mcp_description(&self) -> McpToolDescription {
        McpToolDescription {
            name: self.name.to_string(),
            description: self.description.to_string(),
            input_schema: self.input_schema.clone(),
        }
    }
}

// Implement Debug manually because handler is not Debug
impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish()
    }
}

// ─── ToolContext ──────────────────────────────────────────────────────────────

/// Shared context passed to every tool handler.
/// Contains config, the tool router, lazily-initialized KiCAD clients, and the
/// per-call observer (used by `get_recent_calls` / `server_stats` meta-tools).
pub struct ToolContext {
    pub config: ServerConfig,
    pub router: Arc<ToolRouter>,
    pub observer: crate::observability::CallObserver,
    /// In-memory TTL cache for repeated JLCPCB parts-database queries.
    pub jlcpcb_cache: QueryCache,
}

impl ToolContext {
    /// Construct a context with an in-memory-only observer (no JSONL). Used by
    /// tests and by callers that don't need persistent call logs.
    pub fn new(config: ServerConfig, router: Arc<ToolRouter>) -> Self {
        ToolContext {
            config,
            router,
            observer: crate::observability::CallObserver::new(None),
            jlcpcb_cache: QueryCache::default(),
        }
    }

    /// Construct a context with a specific observer — wired in by `McpHandler`
    /// so the JSONL log and in-memory ring are shared across all tool calls.
    pub fn new_with_observer(
        config: ServerConfig,
        router: Arc<ToolRouter>,
        observer: crate::observability::CallObserver,
    ) -> Self {
        ToolContext {
            config,
            router,
            observer,
            jlcpcb_cache: QueryCache::default(),
        }
    }
}

// ─── QueryCache ───────────────────────────────────────────────────────────────

/// A small in-memory, TTL-based cache for repeated read-only query results
/// (JSON values keyed by a caller-constructed string). One instance lives on
/// `ToolContext` for the life of the server, shared across all tool calls.
pub struct QueryCache {
    ttl: std::time::Duration,
    entries: std::sync::Mutex<std::collections::HashMap<String, (Value, std::time::Instant)>>,
}

impl QueryCache {
    pub fn new(ttl: std::time::Duration) -> Self {
        QueryCache {
            ttl,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Returns a cached value for `key` if present and not yet expired.
    pub fn get(&self, key: &str) -> Option<Value> {
        let entries = self.entries.lock().unwrap();
        entries.get(key).and_then(|(value, inserted_at)| {
            if inserted_at.elapsed() < self.ttl {
                Some(value.clone())
            } else {
                None
            }
        })
    }

    /// Stores `value` under `key`, overwriting any existing (possibly expired) entry.
    pub fn put(&self, key: String, value: Value) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(key, (value, std::time::Instant::now()));
    }
}

impl Default for QueryCache {
    /// 5-minute TTL — long enough to skip redundant re-queries within a single
    /// design session, short enough that a `download_jlcpcb_database` refresh
    /// is reflected without needing an explicit cache-invalidation hook.
    fn default() -> Self {
        QueryCache::new(std::time::Duration::from_secs(300))
    }
}

// ─── ServerConfig ─────────────────────────────────────────────────────────────

/// Subset of the server configuration relevant to tool execution.
/// This is the config that flows from `konnect::Config` into the core crate.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub kicad_cli: String,
    pub kicad_binary: String,
    pub ipc_address: String,
    pub project_dir: Option<std::path::PathBuf>,
    pub jlcpcb_db_path: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod query_cache_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn miss_on_unknown_key() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn put_then_get_roundtrips() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        cache.put("key".to_string(), json!({ "count": 3 }));
        assert_eq!(cache.get("key"), Some(json!({ "count": 3 })));
    }

    #[test]
    fn entry_expires_after_ttl() {
        let cache = QueryCache::new(std::time::Duration::from_millis(10));
        cache.put("key".to_string(), json!("value"));
        assert_eq!(cache.get("key"), Some(json!("value")));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(cache.get("key").is_none());
    }

    #[test]
    fn put_overwrites_existing_entry() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        cache.put("key".to_string(), json!("first"));
        cache.put("key".to_string(), json!("second"));
        assert_eq!(cache.get("key"), Some(json!("second")));
    }
}

// ─── Helper macro for defining tools ─────────────────────────────────────────

/// Shorthand for building a ToolDef with a typed async handler function.
///
/// Usage:
/// ```rust
/// tool!(
///     "tool_name",
///     "Description of what it does.",
///     json_schema,        // serde_json::Value
///     |args, ctx| async move {
///         // handler body
///         Ok(CallToolResult::text("done"))
///     }
/// )
/// ```
#[macro_export]
macro_rules! tool {
    ($name:expr, $desc:expr, $schema:expr, $handler:expr) => {{
        let h: $crate::tools::ToolHandlerFn = std::sync::Arc::new(move |args, ctx| {
            let args = args.clone();
            let ctx = ctx.clone();
            Box::pin(async move { ($handler)(&args, &*ctx).await })
        });
        $crate::tools::ToolDef {
            name: $name,
            description: $desc,
            input_schema: $schema,
            handler: h,
        }
    }};
}

// ─── Argument helpers ─────────────────────────────────────────────────────────

/// Build a structured `InvalidArgument` CallToolResult. Used by the
/// `require_*` helpers so every handler that uses them emits structured
/// errors the client / observer can match on — no per-handler change needed.
fn invalid_arg(field: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.to_string(),
        },
        format!("Argument '{}' is invalid: {}", field, reason),
    )
}

/// Extract a required string argument, returning a structured
/// `InvalidArgument` error result if missing or not a string.
pub fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, CallToolResult> {
    args[key]
        .as_str()
        .ok_or_else(|| invalid_arg(key, "missing or not a string"))
}

/// Extract an optional string argument.
pub fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str()
}

/// Extract a required f64 argument. Returns a structured `InvalidArgument`
/// error result if missing or not a number.
pub fn require_f64(args: &Value, key: &str) -> Result<f64, CallToolResult> {
    args[key]
        .as_f64()
        .ok_or_else(|| invalid_arg(key, "missing or not a number"))
}

/// Extract an optional f64.
pub fn opt_f64(args: &Value, key: &str) -> Option<f64> {
    args[key].as_f64()
}

/// Extract a required path string and return it as a PathBuf, using
/// `anyhow::Error`. Use this variant with `?` inside handlers that return
/// `anyhow::Result`. The surrounding dispatch will stringify the error and
/// surface it as `ToolErrorKind::HandlerError`.
pub fn get_path(args: &Value, key: &str) -> anyhow::Result<std::path::PathBuf> {
    let s = args[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: '{}'", key))?;
    Ok(std::path::PathBuf::from(s))
}

/// Project name used in symbol/sheet `(instances (project "..." ...))` entries:
/// the schematic's file stem, matching what eeschema writes when it saves a
/// standalone root sheet.
pub fn project_name_for(sch_path: &std::path::Path) -> String {
    sch_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Minimal valid blank schematic, with a freshly generated root `(uuid ...)`.
/// The root UUID is mandatory: KiCAD's netlister resolves symbol instance
/// paths against it and silently forms no wire-only nets when it's missing.
pub fn blank_schematic_template() -> String {
    format!(
        "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(uuid \"{}\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n",
        konnect_sexp::writer::new_uuid()
    )
}

/// Root UUID of a loaded schematic, assigning a fresh one when the file
/// predates Konnect writing root UUIDs — the file is repaired on its next
/// overwrite. Instance paths are built as "/<root-uuid>[/<sheet-uuid>…]".
pub fn ensure_root_uuid(sch: &mut konnect_schematic_editor::Schematic) -> String {
    match &sch.uuid {
        Some(u) => u.clone(),
        None => {
            let u = konnect_sexp::writer::new_uuid();
            sch.uuid = Some(u.clone());
            u
        }
    }
}

#[cfg(test)]
mod arg_helper_tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use serde_json::json;

    #[test]
    fn require_str_missing_produces_structured_invalid_argument() {
        let args = json!({});
        let err = require_str(&args, "path").expect_err("should fail");
        assert!(err.is_error);
        assert_eq!(
            extract_error_kind(&err).as_deref(),
            Some("invalid_argument")
        );
        // The body carries the field name so clients can branch.
        let body = match &err.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["field"], "path");
    }

    #[test]
    fn require_f64_non_number_produces_structured_invalid_argument() {
        let args = json!({ "x": "not a number" });
        let err = require_f64(&args, "x").expect_err("should fail");
        assert_eq!(
            extract_error_kind(&err).as_deref(),
            Some("invalid_argument")
        );
    }

    #[test]
    fn require_str_present_returns_value() {
        let args = json!({ "name": "ok" });
        let v = require_str(&args, "name").expect("should parse");
        assert_eq!(v, "ok");
    }
}

// ─── KiCAD config directory detection ────────────────────────────────────────

/// Find the KiCAD user config directory by probing for installed version directories.
/// Checks versions in descending order: 10.0, 9.0, 8.0, then bare "kicad".
pub fn kicad_config_dir() -> std::path::PathBuf {
    let base = kicad_config_base();
    let versions = ["10.0", "9.0", "8.0"];
    for ver in &versions {
        let dir = base.join(ver);
        if dir.is_dir() {
            return dir;
        }
    }
    // Fallback: bare kicad dir or 10.0 (will be created on first use)
    base.join("10.0")
}

/// Platform-specific base directory for KiCAD configs.
fn kicad_config_base() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        std::path::PathBuf::from(appdata).join("kicad")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home)
            .join("Library")
            .join("Preferences")
            .join("kicad")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home).join(".config").join("kicad")
    }
}

// ─── KiCAD symbol library resolution ────────────────────────────────────────

/// Resolve a lib_id like "Device:R" to the full symbol S-expression definition.
/// KiCAD 10 stores symbols in .kicad_symdir directories, one .kicad_sym file per symbol.
/// Returns the symbol block with the lib_id prefix (e.g. "Device:R") as the symbol name.
pub fn resolve_lib_symbol(lib_id: &str) -> Option<String> {
    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        tracing::warn!(
            "[BETA] Cannot resolve lib_id '{}' — expected 'Library:Symbol' format",
            lib_id
        );
        return None;
    }
    let (library_name, symbol_name) = (parts[0], parts[1]);

    let sym_dirs = find_kicad_symbol_dirs();

    for base_dir in &sym_dirs {
        // KiCAD 10: Library.kicad_symdir/SymbolName.kicad_sym
        let symdir_path = base_dir.join(format!("{}.kicad_symdir", library_name));
        let sym_file = symdir_path.join(format!("{}.kicad_sym", symbol_name));

        if sym_file.exists() {
            tracing::debug!("[BETA] Found symbol file: {}", sym_file.display());
            match std::fs::read_to_string(&sym_file) {
                Ok(content) => {
                    if let Some(sym_block) = extract_symbol_block(&content, symbol_name) {
                        let renamed = sym_block.replacen(
                            &format!("(symbol \"{}\"", symbol_name),
                            &format!("(symbol \"{}:{}\"", library_name, symbol_name),
                            1,
                        );
                        return Some(renamed);
                    }
                }
                Err(e) => tracing::warn!("[BETA] Failed to read {}: {}", sym_file.display(), e),
            }
        }

        // Fallback: KiCAD 8/9 format — Library.kicad_sym (single file)
        let legacy_path = base_dir.join(format!("{}.kicad_sym", library_name));
        if legacy_path.exists() {
            match std::fs::read_to_string(&legacy_path) {
                Ok(content) => {
                    if let Some(sym_block) = extract_symbol_block(&content, symbol_name) {
                        let renamed = sym_block.replacen(
                            &format!("(symbol \"{}\"", symbol_name),
                            &format!("(symbol \"{}:{}\"", library_name, symbol_name),
                            1,
                        );
                        return Some(renamed);
                    }
                }
                Err(e) => tracing::warn!("[BETA] Failed to read {}: {}", legacy_path.display(), e),
            }
        }
    }

    tracing::warn!(
        "[BETA] Symbol '{}' not found in any library directory",
        lib_id
    );
    None
}

/// Extract a top-level (symbol "NAME" ...) block from a .kicad_sym file.
fn extract_symbol_block(content: &str, symbol_name: &str) -> Option<String> {
    let pattern = format!("(symbol \"{}\"", symbol_name);
    let start = content.find(&pattern)?;
    let mut depth = 0i32;
    let mut end = start;
    for (i, ch) in content[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end > start {
        Some(content[start..end].to_string())
    } else {
        None
    }
}

/// Insert a symbol definition into the schematic's lib_symbols section.
/// Creates the lib_symbols section if it doesn't exist. Skips if already present.
pub fn ensure_lib_symbol_in_schematic(content: &mut String, lib_id: &str) {
    // Check if already present
    let lib_id_check = format!("(symbol \"{}\"", lib_id);
    if content.contains(&lib_id_check) {
        return;
    }

    // Resolve the symbol from KiCAD libraries
    let sym_def = match resolve_lib_symbol(lib_id) {
        Some(s) => s,
        None => return,
    };

    // Ensure lib_symbols section exists
    if !content.contains("(lib_symbols") {
        if let Some(insert_after) = content.find(")\n") {
            content.insert_str(insert_after + 2, "\n\t(lib_symbols\n\t)\n");
        }
    }

    // Find the closing paren of lib_symbols and insert before it
    if let Some(ls_start) = content.find("(lib_symbols") {
        let mut depth = 0i32;
        let mut ls_end = ls_start;
        for (i, ch) in content[ls_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        ls_end = ls_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let indented = sym_def
            .lines()
            .map(|l| {
                if l.is_empty() {
                    String::new()
                } else {
                    format!("\t\t{}", l)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        content.insert_str(ls_end, &format!("\n{}\n\t", indented));
    }
}

/// Find directories where KiCAD symbol libraries are stored.
fn find_kicad_symbol_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("KICAD10_SYMBOL_DIR") {
        let p = std::path::PathBuf::from(&dir);
        if p.is_dir() {
            dirs.push(p);
        }
    }
    #[cfg(target_os = "windows")]
    {
        let candidates = [
            r"C:\KiCad\10.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\10.0\share\kicad\symbols",
            r"C:\KiCad\9.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\9.0\share\kicad\symbols",
        ];
        for c in &candidates {
            let p = std::path::PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let candidates = ["/usr/share/kicad/symbols", "/usr/local/share/kicad/symbols"];
        for c in &candidates {
            let p = std::path::PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }
    dirs
}
