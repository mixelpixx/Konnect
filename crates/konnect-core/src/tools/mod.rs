//! Tool trait definitions, ToolContext, and all toolset modules.

mod board_session;
pub mod cli;
pub mod config;
pub mod design_review;
mod footprint_graphics;
mod footprint_metadata;
mod footprint_models;
pub mod integration;
pub mod library;
pub mod manufacturing;
pub mod pcb_board;
pub mod pcb_components;
pub mod pcb_export;
pub(crate) mod pcb_footprint_update;
pub mod pcb_routing;
pub(crate) mod pcb_sync;
pub mod placement;
pub mod project;
pub mod sch_analysis;
pub mod sch_batch;
pub mod sch_bus;
pub mod sch_components;
pub(crate) mod sch_connectivity;
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
    /// How the tool interacts with a board held by KiCad. Client guidance can
    /// derive warnings from this runtime contract instead of maintaining a
    /// second list of tool names.
    pub board_access: BoardAccess,
}

/// The board-state contract of a tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoardAccess {
    /// The tool does not need board-state guidance.
    #[default]
    None,
    /// The requested board must be open in a reachable KiCad instance.
    LiveOnly,
    /// Live IPC is preferred, with a guarded file fallback only when KiCad is
    /// unreachable and the board is safe to edit offline.
    LivePreferredWithFallback,
    /// The operation writes the file directly and therefore requires KiCad to
    /// have the board closed.
    ClosedBoardOnly,
    /// Planning is non-mutating, while applying has a stricter board-state
    /// requirement described by the tool itself.
    ApplyModeDependent,
}

impl ToolDef {
    pub fn with_board_access(mut self, board_access: BoardAccess) -> Self {
        self.board_access = board_access;
        self
    }

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
    /// Boards positively observed open through IPC during this server process.
    /// Sticky state prevents an unsafe file fallback after KiCad disappears.
    pub(crate) board_session: board_session::BoardSessionMemory,
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
            board_session: board_session::BoardSessionMemory::default(),
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
            board_session: board_session::BoardSessionMemory::default(),
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
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub kicad_cli: String,
    pub kicad_binary: String,
    pub ipc_address: String,
    pub project_dir: Option<std::path::PathBuf>,
    pub jlcpcb_db_path: Option<std::path::PathBuf>,
    /// Auto-load a tool's toolset on call instead of returning
    /// `toolset_not_loaded`. Off by default (see `konnect::Config::auto_load_toolsets`).
    pub auto_load_toolsets: bool,
    /// Pre-load every toolset at startup so the first `tools/list` is
    /// complete. Off by default (see `konnect::Config::eager_toolsets`).
    pub eager_toolsets: bool,
}

/// Serialises tests that set `KICAD*_DIR`. Those are process-wide and read at
/// call time by `find_kicad_library_dirs`, so two such tests running
/// concurrently see each other's directories.
#[cfg(test)]
pub(crate) static KICAD_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
/// ```rust,ignore
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
            board_access: $crate::tools::BoardAccess::None,
        }
    }};
}

// ─── IPC helpers ──────────────────────────────────────────────────────────────

/// Run `f` against KiCad's IPC API, classifying a failure as
/// transport-unreachable vs KiCad-rejected via [`konnect_ipc::IpcFailure`].
///
/// This is the typed gate for the file-editing fallback — never a text match
/// on the error message — and it is shared rather than copied per toolset:
/// the toolsets' plain `with_ipc` helpers have already drifted from each
/// other, and this is the one decision (is it safe to edit a board file behind
/// a live KiCad?) whose copies must not.
pub async fn with_ipc_classified<T, F>(
    address: String,
    f: F,
) -> anyhow::Result<Result<T, konnect_ipc::IpcFailure>>
where
    T: Send + 'static,
    F: FnOnce(&konnect_ipc::client::KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        f(&konnect_ipc::client::KiCadIpcClient::new(&address))
            .map_err(konnect_ipc::IpcFailure::from_error)
    })
    .await
    {
        Ok(result) => Ok(result),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

/// Run a board-targeted IPC call and remember the requested board as soon as
/// KiCad positively identifies it. Observation happens before `f`, so a later
/// command rejection, timeout, or editor crash cannot make the next file
/// fallback treat this board as never having been live.
pub(crate) async fn with_board_ipc_classified<T, F>(
    ctx: &ToolContext,
    board_path: &std::path::Path,
    f: F,
) -> anyhow::Result<Result<T, konnect_ipc::IpcFailure>>
where
    T: Send + 'static,
    F: FnOnce(&konnect_ipc::client::KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    let requested = board_path.to_path_buf();
    let observation = requested.clone();
    let memory = ctx.board_session.clone();
    with_ipc_classified(ctx.config.ipc_address.clone(), move |client| {
        client.ensure_board_is_active(&requested)?;
        memory.observe_live(&observation);
        f(client)
    })
    .await
}

/// Convert the IPC layer's typed board-target refusal into the stable MCP
/// taxonomy. No handler should flatten these into "KiCad must be running" or
/// a generic rejection: callers need to distinguish a wrong document from an
/// ambiguous or stale one.
pub(crate) fn ipc_target_error_result(error: &konnect_ipc::BoardTargetError) -> CallToolResult {
    use konnect_ipc::BoardTargetError;

    let message = format!("{}. Konnect did not read or modify another board.", error);
    match error {
        BoardTargetError::NoOpenDocuments { requested } => CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::WrongDocument {
                requested: requested.clone(),
                open_documents: Vec::new(),
            },
            message,
        ),
        BoardTargetError::WrongDocument {
            requested,
            open_documents,
        } => CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::WrongDocument {
                requested: requested.clone(),
                open_documents: open_documents.clone(),
            },
            message,
        ),
        BoardTargetError::AmbiguousDocument {
            requested,
            candidates,
        } => CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::AmbiguousTarget {
                target: requested.clone(),
                candidates: candidates.clone(),
            },
            message,
        ),
        BoardTargetError::StaleDocument {
            requested,
            previously_bound,
            open_documents,
        } => CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::StaleTarget {
                target: requested.clone(),
                reason: format!(
                    "previously bound document '{}' is no longer uniquely open; observed [{}]",
                    previously_bound,
                    open_documents.join(", ")
                ),
            },
            message,
        ),
    }
}

#[cfg(test)]
mod ipc_target_error_tests {
    use super::*;

    #[test]
    fn ipc_document_target_failures_keep_distinct_structured_kinds() {
        let cases = [
            (
                konnect_ipc::BoardTargetError::NoOpenDocuments {
                    requested: "target.kicad_pcb".to_string(),
                },
                "wrong_document",
            ),
            (
                konnect_ipc::BoardTargetError::WrongDocument {
                    requested: "target.kicad_pcb".to_string(),
                    open_documents: vec!["other.kicad_pcb".to_string()],
                },
                "wrong_document",
            ),
            (
                konnect_ipc::BoardTargetError::AmbiguousDocument {
                    requested: "target.kicad_pcb".to_string(),
                    candidates: vec![
                        "target.kicad_pcb".to_string(),
                        "target.kicad_pcb".to_string(),
                    ],
                },
                "ambiguous_target",
            ),
            (
                konnect_ipc::BoardTargetError::StaleDocument {
                    requested: "target.kicad_pcb".to_string(),
                    previously_bound: "target.kicad_pcb".to_string(),
                    open_documents: vec!["other.kicad_pcb".to_string()],
                },
                "stale_target",
            ),
        ];

        for (error, expected_kind) in cases {
            let result = ipc_target_error_result(&error);
            assert!(result.is_error);
            assert_eq!(
                crate::mcp::error::extract_error_kind(&result).as_deref(),
                Some(expected_kind)
            );
        }
    }
}

// ─── Argument helpers ─────────────────────────────────────────────────────────

/// Build a structured `InvalidArgument` CallToolResult. Used by the
/// `require_*` helpers so every handler that uses them emits structured
/// errors the client / observer can match on — no per-handler change needed.
pub(crate) fn invalid_arg(field: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.to_string(),
        },
        format!("Argument '{}' is invalid: {}", field, reason),
    )
}

/// The reason string for a footprint file whose root is not `(footprint ...)`.
///
/// A pre-6.0 library file has a `(module ...)` root instead, and those are
/// still everywhere — vendor downloads and older personal libraries. The
/// generic "file root must be a footprint" reads like a dead end there, when
/// one shipped command migrates the file (#304). Name the situation and the
/// way out; keep the generic message for everything else.
pub(crate) fn footprint_root_reason(head: Option<&str>) -> String {
    match head {
        Some("module") => "file root is a pre-6.0 `(module ...)` footprint — \
             convert it with `kicad-cli fp upgrade <dir-or-file>`, then retry"
            .to_string(),
        _ => "file root must be a footprint".to_string(),
    }
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

/// Extract an optional array-of-strings argument: `None` when absent, a
/// structured `InvalidArgument` error when present but not an array of
/// strings. Prefer this over `as_array().unwrap_or_default()`, which reports a
/// malformed argument as an empty list.
pub fn opt_str_list(args: &Value, key: &str) -> Result<Option<Vec<String>>, CallToolResult> {
    match &args[key] {
        Value::Null => Ok(None),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| invalid_arg(key, "every entry must be a string")),
        _ => Err(invalid_arg(key, "expected an array of strings")),
    }
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

/// Extract a required array argument. Returns a structured `InvalidArgument`
/// error result if missing or not an array.
///
/// An *empty* array is accepted: `[]` is a caller saying "operate on nothing",
/// which is a coherent request. Omitting the argument is not — that is the
/// caller forgetting to say what to operate on, and the two must not look the
/// same to a tool that then reports success (#218).
pub fn require_array<'a>(args: &'a Value, key: &str) -> Result<&'a Vec<Value>, CallToolResult> {
    args[key]
        .as_array()
        .ok_or_else(|| invalid_arg(key, "missing or not an array"))
}

/// Extract a required non-negative integer argument. Returns a structured
/// `InvalidArgument` error result if missing or not one.
pub fn require_u64(args: &Value, key: &str) -> Result<u64, CallToolResult> {
    args[key]
        .as_u64()
        .ok_or_else(|| invalid_arg(key, "missing or not a non-negative integer"))
}

/// A required argument was absent or the wrong type.
///
/// Carried inside the `anyhow::Error` that [`get_path`] returns so the MCP
/// dispatch layer can report `invalid_argument` naming the field, the same as
/// [`require_str`], without `get_path`'s 171 call sites changing shape.
///
/// Classify by downcasting, never by matching the message — the same rule
/// `konnect_ipc::TransportUnreachable` follows (#194).
#[derive(Debug)]
pub struct MissingArgument {
    pub field: String,
}

impl std::fmt::Display for MissingArgument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Missing required argument: '{}'", self.field)
    }
}

impl std::error::Error for MissingArgument {}

impl MissingArgument {
    /// The field named by the first [`MissingArgument`] in `error`'s chain.
    pub fn field_in(error: &anyhow::Error) -> Option<&str> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<Self>())
            .map(|missing| missing.field.as_str())
    }
}

/// Extract a required path string and return it as a PathBuf, using
/// `anyhow::Error`. Use this variant with `?` inside handlers that return
/// `anyhow::Result`.
///
/// A missing or non-string argument carries [`MissingArgument`], which the
/// dispatch layer reports as `invalid_argument` naming the field. A path that
/// is present but unusable — absent on disk, wrong extension — is not this
/// error: that is the handler trying and failing, and stays a handler error or
/// a `FileNotFound` (#194).
pub fn get_path(args: &Value, key: &str) -> anyhow::Result<std::path::PathBuf> {
    let s = args[key].as_str().ok_or_else(|| {
        anyhow::Error::new(MissingArgument {
            field: key.to_string(),
        })
    })?;
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
    konnect_sexp::schematic::format_blank_schematic()
}

/// Same, on a caller-chosen paper size — validate the name first.
pub fn blank_schematic_template_with_paper(size: &str, portrait: bool) -> String {
    konnect_sexp::schematic::format_blank_schematic_with_paper(size, portrait)
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

/// Every pin placed on the sheet, paired with the transform that put it there.
///
/// Unit-aware: a multi-unit library symbol superimposes every unit's pins on
/// one placement, so an instance of unit 1 must not report unit 2's pins (#35).
pub(crate) fn placed_pins(
    tree: &konnect_sexp::SexpNode,
) -> Vec<(
    konnect_sexp::schematic::LibPin,
    konnect_sexp::geometry::PinTransform,
)> {
    placed_pins_by_reference(tree)
        .into_iter()
        .flat_map(|(_, pins)| pins)
        .collect()
}

/// [`placed_pins`], grouped under the instance that placed each unit, for
/// callers that report pins by name rather than position. The whole instance
/// is returned because a caller reporting a pin usually wants its reference
/// *and* something else about the component — value, uuid, unit — and a
/// reference alone collapses on a pre-annotation sheet where every part is
/// `R?`.
pub(crate) fn placed_pins_by_reference(
    tree: &konnect_sexp::SexpNode,
) -> Vec<(
    konnect_sexp::schematic::SymbolInstance,
    Vec<(
        konnect_sexp::schematic::LibPin,
        konnect_sexp::geometry::PinTransform,
    )>,
)> {
    use konnect_sexp::schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, find_lib_symbol,
    };
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut by_reference = Vec::new();
    for inst in extract_symbol_instances(tree) {
        // find_lib_symbol, not a lib_id match: an instance carrying a
        // (lib_name …) is a sheet-local derived symbol whose pins can sit
        // elsewhere than the base definition's (#143).
        let Some(sym) = find_lib_symbol(&lib_syms, &inst) else {
            continue;
        };
        let t = inst.pin_transform();
        let pins = extract_lib_pins_for_unit(sym, inst.unit)
            .into_iter()
            .map(|p| (p, t))
            .collect();
        by_reference.push((inst, pins));
    }
    by_reference
}

/// All symbol pin connection points in a parsed schematic tree. These drive
/// junction insertion, and a dot dropped on a phantom pin where two wires
/// cross would short them — hence [`placed_pins`]' unit-awareness.
pub(crate) fn all_pin_endpoints(tree: &konnect_sexp::SexpNode) -> Vec<(f64, f64)> {
    placed_pins(tree)
        .into_iter()
        .map(|(p, t)| konnect_sexp::schematic::pin_endpoint(&p, t))
        .collect()
}

/// The direction leading away from the symbol body at `(x, y)`. `None` when
/// no pin sits there, or when stacked pins disagree about which way is out.
pub(crate) fn pin_outward_at(tree: &konnect_sexp::SexpNode, x: f64, y: f64) -> Option<f64> {
    use konnect_sexp::geometry::points_coincident;
    use konnect_sexp::schematic::{pin_endpoint, pin_outward_direction};
    let mut found: Option<f64> = None;
    for (pin, t) in placed_pins(tree) {
        let (px, py) = pin_endpoint(&pin, t);
        if !points_coincident(px, py, x, y, 0.01) {
            continue;
        }
        let outward = pin_outward_direction(&pin, t);
        match found {
            Some(d) if d != outward => return None,
            _ => found = Some(outward),
        }
    }
    found
}

/// The stub directions, as name, unit offset, and the angle that offset points
/// along. Schematic Y grows downward, so "up" is negative. `"right"` leads:
/// it is the fallback for an unknown name and for an unresolvable `"auto"`.
const STUB_DIRECTIONS: [(&str, f64, f64, f64); 4] = [
    ("right", 1.0, 0.0, 0.0),
    ("up", 0.0, -1.0, 90.0),
    ("left", -1.0, 0.0, 180.0),
    ("down", 0.0, 1.0, 270.0),
];

/// A resolved stub direction: which way the wire leaves the anchor, and how to
/// orient the label at its far end.
pub(crate) struct StubDirection {
    pub name: &'static str,
    /// Unit offset in schematic space (Y grows downward).
    pub dx: f64,
    pub dy: f64,
    pub label_rotation: f64,
}

/// Resolve a `direction` argument against an already-known outward direction.
/// `"auto"` follows `outward`, falling back to `"right"` — the default before
/// `"auto"` existed — when the caller could not determine one.
pub(crate) fn stub_direction(direction: &str, outward: Option<f64>) -> StubDirection {
    use konnect_sexp::schematic::horizontal_label_rotation;
    let row = match direction {
        // Outward angles are snapped to quadrants, so this compares exactly.
        "auto" => outward.and_then(|d| STUB_DIRECTIONS.iter().find(|r| r.3 == d)),
        name => STUB_DIRECTIONS.iter().find(|r| r.0 == name),
    }
    .unwrap_or(&STUB_DIRECTIONS[0]);
    StubDirection {
        name: row.0,
        dx: row.1,
        dy: row.2,
        label_rotation: horizontal_label_rotation(row.3),
    }
}

/// [`stub_direction`] for a caller holding only a coordinate. Naming a pin is
/// exact; matching one by position gives up when stacked pins there disagree.
pub(crate) fn resolve_stub_direction(
    direction: &str,
    anchor: (f64, f64),
    tree: &konnect_sexp::SexpNode,
) -> StubDirection {
    stub_direction(direction, pin_outward_at(tree, anchor.0, anchor.1))
}

/// Add junction dots for pins of `reference` that land mid-segment on a wire.
/// KiCad connects a pin mid-wire only through a junction dot (verified with
/// kicad-cli 10: a junction alone connects; splitting the wire is unnecessary).
/// Returns the junction positions added.
pub(crate) fn add_pin_midwire_junctions(
    sch_path: &std::path::Path,
    reference: &str,
) -> anyhow::Result<Vec<(f64, f64)>> {
    use konnect_sexp::geometry::{point_on_segment, points_coincident};
    use konnect_sexp::schematic::{
        extract_junctions, extract_lib_pins_for_unit, extract_symbol_instances, extract_wires,
        find_lib_symbol, pin_endpoint, read_schematic,
    };
    let tol = 0.01;
    let (_, tree) = read_schematic(sch_path)?;
    let wires = extract_wires(&tree);
    if wires.is_empty() {
        return Ok(Vec::new());
    }
    let junctions = extract_junctions(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut to_add: Vec<(f64, f64)> = Vec::new();
    for inst in extract_symbol_instances(&tree)
        .iter()
        .filter(|i| i.reference == reference)
    {
        let Some(sym) = find_lib_symbol(&lib_syms, inst) else {
            continue;
        };
        let t = inst.pin_transform();
        // Unit-aware for the same reason as all_pin_endpoints: this one writes
        // to the user's file, so a phantom-pin junction is a real defect.
        for pin in extract_lib_pins_for_unit(sym, inst.unit) {
            let (px, py) = pin_endpoint(&pin, t);
            let mid_wire = wires.iter().any(|w| {
                point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol)
                    && !points_coincident(px, py, w.x1, w.y1, tol)
                    && !points_coincident(px, py, w.x2, w.y2, tol)
            });
            let already = junctions
                .iter()
                .chain(to_add.iter())
                .any(|(jx, jy)| points_coincident(px, py, *jx, *jy, tol));
            if mid_wire && !already {
                to_add.push((px, py));
            }
        }
    }
    if !to_add.is_empty() {
        let mut sch = konnect_schematic_editor::Schematic::load(sch_path)?;
        for &(x, y) in &to_add {
            sch.add_junction(x, y);
        }
        sch.overwrite()?;
    }
    Ok(to_add)
}

/// A symbol-instance property positioned in absolute sheet coordinates, with
/// eeschema's default 1.27mm font. The `(at)` node is mandatory: a property
/// written without one is defaulted to the sheet origin by KiCAD, which is how
/// every `#PWR` reference used to pile up in the top-left corner (PR #95).
///
/// Hidden properties get KiCAD 10's property-level `(hide yes)` — a sibling
/// before `(effects)`, exactly as eeschema writes instances (PR #96); the
/// legacy hide-inside-effects form renders the same but round-trips dirty.
///
/// `justify` comes from the library field and is written through unchanged,
/// like the angle in [`field_at`]: it is expressed in the text's own frame, so
/// it stays true however the instance is rotated. Centred fields write no
/// `(justify …)`, which is how KiCad spells centred.
pub(crate) fn positioned_property(
    name: &str,
    value: &str,
    x: f64,
    y: f64,
    rotation: f64,
    hide: bool,
    justify: konnect_schematic_editor::library::FieldJustify,
) -> konnect_schematic_editor::Property {
    use konnect_schematic_editor::sexp::{atom, SexpNode};
    use konnect_schematic_editor::types::fmt_f64;

    let mut prop = konnect_schematic_editor::Property::new(name, value);
    prop.sub_nodes.push(SexpNode::List(vec![
        atom("at"),
        atom(fmt_f64(x)),
        atom(fmt_f64(y)),
        atom(fmt_f64(rotation)),
    ]));
    if hide {
        prop.sub_nodes
            .push(SexpNode::List(vec![atom("hide"), atom("yes")]));
    }
    let mut effects = vec![
        atom("effects"),
        SexpNode::List(vec![
            atom("font"),
            SexpNode::List(vec![atom("size"), atom("1.27"), atom("1.27")]),
        ]),
    ];
    let tokens = justify.tokens();
    if !tokens.is_empty() {
        let mut node = vec![atom("justify")];
        node.extend(tokens.into_iter().map(atom));
        effects.push(SexpNode::List(node));
    }
    prop.sub_nodes.push(SexpNode::List(effects));
    prop
}

/// Sheet-space `(x, y, rotation)` for one instance field, from its library
/// anchor (#101).
///
/// The two halves are stored differently, which is easy to get backwards:
///
/// - **Position is absolute.** The anchor is library space (Y-up), the file
///   wants sheet space (Y-down), so it goes through the same
///   flip-rotate-mirror-translate as a pin —
///   [`transform_pin`](konnect_sexp::geometry::transform_pin) is that math.
///   This is what carries a label around with a rotated body instead of
///   leaving it beside the wrong edge.
/// - **Angle is relative.** KiCad adds the symbol's own rotation to a field's
///   stored angle when it draws, so the library value is written through
///   unchanged. Rotating it here too would double-count: verified by
///   rendering a 90°-rotated `Device:R` with `kicad-cli sch export svg` —
///   stored 0° draws the reference *vertically* over the horizontal body,
///   stored 90° (the library's own value) draws it horizontally above it.
///
/// `fallback` is a library-space anchor too, used when the library defines
/// none, so both halves behave identically either way.
///
/// The angle folds into 0°..180°: a field is horizontal or vertical, never
/// upside down.
pub(crate) fn field_at(
    anchor: Option<(f64, f64, f64)>,
    fallback: (f64, f64, f64),
    t: konnect_sexp::geometry::PinTransform,
) -> (f64, f64, f64) {
    let (ax, ay, arot) = anchor.unwrap_or(fallback);
    let (x, y) = konnect_sexp::geometry::transform_pin(ax, ay, t);
    (x, y, arot.rem_euclid(180.0))
}

/// Library-space fallback anchors matching the pre-#101 hardcoded placement:
/// Reference 3.81mm above the origin, Value 3.81mm below. Y is negated on the
/// way to sheet coords, hence the sign flip against the old literals.
pub(crate) const FALLBACK_REFERENCE_AT: (f64, f64, f64) = (0.0, 3.81, 0.0);
pub(crate) const FALLBACK_VALUE_AT: (f64, f64, f64) = (0.0, -3.81, 0.0);

// ─── Schematic text helpers ──────────────────────────────────────────────────

/// Byte range of the placed `(symbol …)` block whose Reference property is
/// `reference`, for the text-editing tool paths.
///
/// Works regardless of indentation — eeschema saves with tabs, this crate's
/// writer uses two spaces — and skips library definitions inside `lib_symbols`,
/// which carry a Reference property of their own (`"R"`, `"#PWR"`, or whatever
/// a hand-authored library sets) but never a `lib_id`. Only placed instances
/// have one, so that's the discriminator.
pub fn find_symbol_instance_block(content: &str, reference: &str) -> Option<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .next()
}

/// Byte ranges of *every* placed `(symbol …)` block whose Reference property is
/// `reference`, in file order.
///
/// A multi-unit part is placed as one instance **per unit**, and every instance
/// repeats the same reference — a 74HC14 is seven `U6` blocks. Anything the
/// units share rather than own (a field value, the part's very existence) has to
/// be applied to all of them: eeschema writes a field edit into every unit, and
/// deleting one unit's block leaves the rest behind as orphans. Use this rather
/// than [`find_symbol_instance_block`] wherever the operation is about the
/// *component*; the singular form is for operations about one placement.
pub fn find_all_symbol_instance_blocks(content: &str, reference: &str) -> Vec<(usize, usize)> {
    let ref_search = format!(r#"(property "Reference" "{reference}""#);
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut from = 0usize;

    while let Some(rel) = content[from..].find(&ref_search) {
        let ref_pos = from + rel;
        if let Some((start, end)) =
            konnect_sexp::writer::find_enclosing_block(content, "symbol", ref_pos)
        {
            // Skip lib_symbols definitions: they carry a Reference property of
            // their own but never a lib_id.
            if content[start..end].contains("(lib_id ") && !blocks.iter().any(|&(s, _)| s == start)
            {
                blocks.push((start, end));
            }
        }
        from = ref_pos + ref_search.len();
    }
    blocks
}

#[cfg(test)]
mod symbol_block_tests {
    use super::*;

    /// Instance blocks as eeschema writes them: tab-indented, and preceded by a
    /// lib_symbols definition carrying its own Reference property.
    const EESCHEMA_STYLE: &str = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\"\n\t\t\t\t(at 2.032 0 90)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 102 82 0)\n\t\t)\n\t)\n)\n";

    /// Same shape, two-space indented, as this crate's writer emits.
    const KONNECT_STYLE: &str = "(kicad_sch\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\"\n        (at 2.032 0 90)\n      )\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 100 80 0)\n    (property \"Reference\" \"R1\"\n      (at 102 78 0)\n    )\n  )\n)\n";

    #[test]
    fn finds_instance_in_tab_indented_file() {
        let (start, end) = find_symbol_instance_block(EESCHEMA_STYLE, "R1").expect("R1 block");
        let block = &EESCHEMA_STYLE[start..end];
        assert!(block.starts_with("(symbol"));
        assert!(block.contains("(lib_id \"Device:R\")"));
        assert!(block.contains("\"R1\""));
        assert!(
            block.contains("\"10k\""),
            "block must span the whole symbol"
        );
    }

    #[test]
    fn finds_instance_in_space_indented_file() {
        let (start, end) = find_symbol_instance_block(KONNECT_STYLE, "R1").expect("R1 block");
        assert!(KONNECT_STYLE[start..end].contains("(lib_id \"Device:R\")"));
    }

    #[test]
    fn library_definition_is_not_mistaken_for_an_instance() {
        // A hand-authored library whose default Reference matches a placed
        // instance's designator must not shadow the instance.
        let sch = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Custom:Thing\"\n\t\t\t(property \"Reference\" \"U1\"\n\t\t\t\t(at 0 0 0)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Custom:Thing\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 5 5 0)\n\t\t)\n\t)\n)\n";
        let (start, end) = find_symbol_instance_block(sch, "U1").expect("instance");
        assert!(
            sch[start..end].contains("(lib_id "),
            "must skip the lib_symbols definition and return the placed instance"
        );
    }

    #[test]
    fn unknown_reference_is_none() {
        assert!(find_symbol_instance_block(EESCHEMA_STYLE, "R99").is_none());
    }

    #[test]
    fn reference_prefix_does_not_match_longer_designator() {
        // "R1" must not match the R12 instance.
        let sch = "(kicad_sch\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(property \"Reference\" \"R12\"\n\t\t\t(at 1 1 0)\n\t\t)\n\t)\n)\n";
        assert!(find_symbol_instance_block(sch, "R1").is_none());
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

// ─── lib_symbols embedding ──────────────────────────────────────────────────

/// Structured "this lib_id doesn't exist" error, with did-you-mean hints —
/// silently accepting an unresolvable lib_id writes a netlist-invisible
/// component with an empty pin list (#34).
pub fn lib_symbol_not_found_error(
    lib_id: &str,
    src: &dyn konnect_schematic_editor::library::SymbolLibrarySource,
) -> CallToolResult {
    let library = lib_id.split(':').next().unwrap_or(lib_id);
    let mut msg = if !konnect_schematic_editor::library::library_exists(library, src) {
        // Naming only KICAD10_SYMBOL_DIR misleads when the library *is*
        // registered — the tables are the primary source.
        format!(
            "Library '{}' not found in the project or global sym-lib-table, nor as \
             '{}.kicad_symdir'/'{}.kicad_sym' in the installed KiCad symbol \
             libraries (lib_id '{}'). Register it with register_symbol_library, \
             or set KICAD10_SYMBOL_DIR for a non-standard install.",
            library, library, library, lib_id
        )
    } else {
        format!(
            "Library symbol '{}' not found in library '{}'.",
            lib_id, library
        )
    };
    let suggestions = konnect_schematic_editor::library::suggest_symbols(lib_id, 3, src);
    if !suggestions.is_empty() {
        msg.push_str(&format!(
            " Did you mean: {}? (KiCAD 10 renamed several older symbol names)",
            suggestions.join(", ")
        ));
    }
    CallToolResult::error(msg)
}

/// Insert a symbol definition into the schematic's lib_symbols section.
/// Creates the lib_symbols section if it doesn't exist. Skips if already present.
///
/// Returns `false` when `lib_id` cannot be resolved — callers must surface
/// that as an error rather than writing a definition-less instance (#34).
#[must_use]
pub fn ensure_lib_symbol_in_schematic(
    content: &mut String,
    lib_id: &str,
    src: &dyn konnect_schematic_editor::library::SymbolLibrarySource,
) -> bool {
    // Check if already present
    let lib_id_check = format!("(symbol \"{}\"", lib_id);
    if content.contains(&lib_id_check) {
        return true;
    }

    // Flattened: a derived symbol must be embedded with its parent's units
    // copied in, not as a stub kicad-cli can't netlist (#35).
    let sym_def = match konnect_schematic_editor::library::resolve_lib_symbol_flattened(lib_id, src)
    {
        Some(s) => s,
        None => return false,
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
        content.insert_str(ls_end, &format!("\n{}\n\t", indent_lib_symbol(&sym_def)));
    }
    true
}

/// A resolved library definition indented to sit inside `lib_symbols`. Shared
/// so an embedded copy can be compared against the library it came from.
fn indent_lib_symbol(sym_def: &str) -> String {
    sym_def
        .lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("\t\t{}", l)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Outcome of re-embedding one symbol definition.
pub(crate) enum ReembedOutcome {
    Updated,
    /// The embedded copy already matches the library.
    Unchanged,
    /// The library no longer resolves this lib_id.
    Unresolved,
    /// The schematic has no embedded copy to replace.
    NotEmbedded,
    /// The library moved or removed pin anchors, so the update was refused:
    /// wires and labels attach at pin coordinates, and refreshing the
    /// definition under them would silently orphan them (#177). Carries a
    /// human-readable description per affected pin.
    PinsMoved(Vec<String>),
}

/// Replace each embedded definition in `lib_ids` with the library's current
/// one, returning an outcome per entry in the same order.
///
/// [`ensure_lib_symbol_in_schematic`] deliberately leaves an existing copy
/// alone, so a symbol edited in its library keeps rendering from the stale
/// copy — what KiCad reports as "doesn't match copy in library". This is the
/// explicit refresh, mirroring eeschema's "Update Symbols from Library".
///
/// Takes the whole batch so `lib_symbols` is located once rather than per
/// symbol, and so the edits can be applied back to front against offsets that
/// stay valid.
pub(crate) fn reembed_lib_symbols(
    content: &mut String,
    lib_ids: &[String],
    allow_pin_moves: bool,
    src: &dyn konnect_schematic_editor::library::SymbolLibrarySource,
) -> Vec<ReembedOutcome> {
    let blocks = konnect_sexp::writer::find_direct_child_blocks(content, "lib_symbols");
    let mut outcomes = Vec::with_capacity(lib_ids.len());
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    for lib_id in lib_ids {
        let Some(&(start, end)) = blocks
            .iter()
            .find(|&&(s, e)| lib_symbol_name(&content[s..e]) == Some(lib_id.as_str()))
        else {
            outcomes.push(ReembedOutcome::NotEmbedded);
            continue;
        };
        // Flattened, same as the embed path: a derived symbol's copy must
        // carry its parent's units, not an (extends …) stub (#35).
        let Some(sym_def) =
            konnect_schematic_editor::library::resolve_lib_symbol_flattened(lib_id, src)
        else {
            outcomes.push(ReembedOutcome::Unresolved);
            continue;
        };
        // The leading indentation is already in place before `start`.
        let indented = indent_lib_symbol(&sym_def);
        let fresh = indented.trim_start();
        // Compare parsed, not byte for byte: the two embed paths lay a
        // definition out differently, and reflowing one is not an update worth
        // writing. A block that won't parse counts as changed — and forfeits
        // the pin guard below, which needs both trees to compare anchors.
        if let (Ok(embedded), Ok(library)) = (
            konnect_sexp::parse_sexp(&content[start..end]),
            konnect_sexp::parse_sexp(fresh),
        ) {
            if embedded == library {
                outcomes.push(ReembedOutcome::Unchanged);
                continue;
            }
            if !allow_pin_moves {
                let moved = moved_pin_anchors(&embedded, &library);
                if !moved.is_empty() {
                    outcomes.push(ReembedOutcome::PinsMoved(moved));
                    continue;
                }
            }
        }
        edits.push((start, end, fresh.to_string()));
        outcomes.push(ReembedOutcome::Updated);
    }

    edits.sort_by_key(|&(start, ..)| std::cmp::Reverse(start));
    for (start, end, fresh) in edits {
        content.replace_range(start..end, &fresh);
    }
    outcomes
}

/// The quoted name in a `(symbol "Lib:Name" …)` definition.
///
/// The name may sit on the same line as `(symbol` or on the next one depending
/// on which embed path wrote it, so this skips whitespace rather than
/// pattern-matching a single layout.
fn lib_symbol_name(block: &str) -> Option<&str> {
    block
        .strip_prefix("(symbol")?
        .trim_start()
        .strip_prefix('"')?
        .split('"')
        .next()
}

/// Every pin anchor in a symbol definition: `(number, x, y)` from each
/// `(pin … (at x y angle) … (number "N"))`, at any nesting depth so both
/// single- and multi-unit bodies are covered. Duplicates are kept — stacked
/// power pins share a number, and losing one of them is still a move.
fn pin_anchors(def: &konnect_sexp::SexpNode) -> Vec<(String, f64, f64)> {
    let mut anchors = Vec::new();
    let mut stack = vec![def];
    while let Some(node) = stack.pop() {
        for pin in node.find_all("pin") {
            let Some(at) = pin.find("at") else { continue };
            let (Some(x), Some(y)) = (at.get_f64(1), at.get_f64(2)) else {
                continue;
            };
            let number = pin
                .find("number")
                .and_then(|n| n.get(1))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            anchors.push((number, x, y));
        }
        stack.extend(node.find_all("symbol"));
    }
    anchors
}

/// Anchors present in `old_def` that `new_def` no longer has, as
/// human-readable descriptions. Empty means every existing pin kept its
/// position — the safe case for an in-place refresh, since wires and labels
/// attach at pin coordinates. New pins are not moves: nothing attaches to a
/// pin that didn't exist.
fn moved_pin_anchors(
    old_def: &konnect_sexp::SexpNode,
    new_def: &konnect_sexp::SexpNode,
) -> Vec<String> {
    let mut remaining = pin_anchors(new_def);
    let mut moves = Vec::new();
    for (number, x, y) in pin_anchors(old_def) {
        if let Some(i) = remaining
            .iter()
            .position(|(n, nx, ny)| *n == number && (nx - x).abs() < 1e-6 && (ny - y).abs() < 1e-6)
        {
            remaining.swap_remove(i);
            continue;
        }
        match remaining.iter().find(|(n, ..)| *n == number) {
            Some((_, nx, ny)) => moves.push(format!(
                "pin {number} moved from ({x}, {y}) to ({nx}, {ny})"
            )),
            None => moves.push(format!("pin {number} at ({x}, {y}) was removed")),
        }
    }
    moves
}

/// Roots under which KiCAD ships its bundled libraries — the directory that
/// directly contains `symbols/`, `footprints/` and `3dmodels/`.
fn kicad_share_roots() -> Vec<std::path::PathBuf> {
    crate::kicad_install::share_roots()
}

/// Find directories holding a bundled KiCAD library kind — `"symbols"`,
/// `"footprints"` or `"3dmodels"`.
///
/// The matching environment variable wins when KiCad has exported it (it does
/// so for plugins); otherwise the well-known install locations are searched,
/// newest KiCad first. The names are not a plain uppercasing of `kind` — they
/// are singular, and the 3D one is not a word:
///
/// | `kind`        | variable                |
/// |---------------|-------------------------|
/// | `symbols`     | `KICAD<major>_SYMBOL_DIR`    |
/// | `footprints`  | `KICAD<major>_FOOTPRINT_DIR` |
/// | `3dmodels`    | `KICAD<major>_3DMODEL_DIR`   |
pub(crate) fn find_kicad_library_dirs(kind: &str) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut push = |p: std::path::PathBuf| {
        if p.is_dir() && !dirs.contains(&p) {
            dirs.push(p);
        }
    };

    if let Some(suffix) = kicad_env_suffix(kind) {
        for major in ["10", "9", "8"] {
            // var_os, not var: a directory whose name is not valid Unicode is
            // still a directory KiCad may have pointed us at, and `var` reports
            // those as absent — silently falling back to the install roots, or
            // to nothing, on exactly the machines where the variable was the
            // only correct answer.
            if let Some(dir) = std::env::var_os(format!("KICAD{major}_{suffix}")) {
                push(std::path::PathBuf::from(dir));
            }
        }
    }
    for root in kicad_share_roots() {
        push(root.join(kind));
    }
    dirs
}

/// The `KICAD<major>_…` environment-variable suffix naming a library kind.
fn kicad_env_suffix(kind: &str) -> Option<&'static str> {
    match kind {
        "symbols" => Some("SYMBOL_DIR"),
        "footprints" => Some("FOOTPRINT_DIR"),
        "3dmodels" => Some("3DMODEL_DIR"),
        _ => None,
    }
}

/// Where a sheet sits in its project's hierarchy: the project name and the
/// instance path eeschema would key its symbols to.
///
/// A symbol's `(instances (project "NAME" (path "/…")))` entry is what KiCad
/// reads the designator from, and both halves are properties of the **root**
/// sheet, not of the file the symbol happens to live in. Deriving them from
/// the child file — its own stem as the project name, its own uuid as the
/// whole path — produces an entry KiCad matches against nothing, so the
/// symbol reads as unannotated on that sheet (#204).
pub struct SheetInstanceContext {
    /// Project name: the `.kicad_pro` stem, falling back to the root sheet's.
    pub project_name: String,
    /// `/root-uuid[/sheet-uuid…]`, the path from the root down to this sheet.
    pub instance_path: String,
    /// Every structurally observed path to this document. A reused child sheet
    /// has one entry per hierarchy instance; document-wide edits affect all of
    /// them and must never silently choose the first.
    pub instance_paths: Vec<String>,
    /// Whether this sheet was reached from a root other than itself.
    pub is_child_sheet: bool,
}

/// One structurally proven project owner of a schematic file.
///
/// Ownership is not inferred from directory ancestry alone. A child sheet is
/// owned only when a candidate project's root schematic reaches it through
/// parsed `(sheet (property "Sheetfile" ...))` nodes. This is the authority
/// bound for the ancestor walk: it may inspect every ancestor so deeply nested
/// sheets continue to work, but an unrelated project can never win merely by
/// being higher in the filesystem (#189).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchematicOwnership {
    pub project_file: std::path::PathBuf,
    pub root_schematic: std::path::PathBuf,
    pub instance_paths: Vec<String>,
}

/// A schematic target could not be resolved without choosing arbitrarily.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchematicTargetError {
    AmbiguousProject {
        target: std::path::PathBuf,
        roots: Vec<std::path::PathBuf>,
    },
    StaleTarget {
        target: std::path::PathBuf,
        reason: String,
    },
}

impl std::fmt::Display for SchematicTargetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousProject { target, roots } => write!(
                formatter,
                "schematic '{}' belongs to multiple project roots: {}",
                target.display(),
                roots
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::StaleTarget { target, reason } => {
                write!(
                    formatter,
                    "schematic '{}' is stale: {reason}",
                    target.display()
                )
            }
        }
    }
}

impl std::error::Error for SchematicTargetError {}

impl SchematicTargetError {
    pub(crate) fn into_tool_result(self) -> CallToolResult {
        match self {
            Self::AmbiguousProject { target, roots } => {
                let candidates = roots
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect::<Vec<_>>();
                CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::AmbiguousTarget {
                        target: target.display().to_string(),
                        candidates: candidates.clone(),
                    },
                    format!(
                        "Schematic '{}' is reachable from multiple project roots ({}). \
                         Provide a document inside one unambiguous project; Konnect did not \
                         choose one or modify the schematic.",
                        target.display(),
                        candidates.join(", ")
                    ),
                )
            }
            Self::StaleTarget { target, reason } => CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::StaleTarget {
                    target: target.display().to_string(),
                    reason: reason.clone(),
                },
                format!(
                    "Schematic '{}' does not match its structurally observed target state: {}. \
                     Konnect did not modify the schematic.",
                    target.display(),
                    reason
                ),
            ),
        }
    }
}

/// Resolve the unique project whose parsed sheet hierarchy owns `target`.
///
/// A project beside a root schematic is exact. A project above a child is a
/// candidate only when its `<project>.kicad_sch` structurally reaches the
/// target. Missing/unreadable roots and unrelated projects are ignored. If
/// more than one parsed root reaches the target, resolution refuses and names
/// every root instead of depending on `read_dir` order.
pub(crate) fn resolve_schematic_ownership(
    target: &std::path::Path,
) -> Result<Option<SchematicOwnership>, SchematicTargetError> {
    use std::collections::BTreeSet;

    let Some(start) = target.parent() else {
        return Ok(None);
    };
    let mut project_files = BTreeSet::new();
    for directory in start.ancestors() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("kicad_pro"))
            {
                project_files.insert(path);
            }
        }
    }

    let mut owners = Vec::new();
    for project_file in project_files {
        let root_schematic = project_file.with_extension("kicad_sch");
        let Ok(root) = konnect_schematic_editor::Schematic::load(&root_schematic) else {
            continue;
        };
        let Some(root_uuid) = root.uuid.clone() else {
            continue;
        };

        let mut instance_paths = Vec::new();
        if same_schematic_document(&root_schematic, target) {
            instance_paths.push(format!("/{root_uuid}"));
        } else {
            let mut suffix = Vec::new();
            let mut stack = std::collections::HashSet::new();
            collect_sheet_instance_paths(
                &root_schematic,
                target,
                &mut suffix,
                &mut stack,
                0,
                &root_uuid,
                &mut instance_paths,
            );
        }
        instance_paths.sort();
        instance_paths.dedup();
        if !instance_paths.is_empty() {
            owners.push(SchematicOwnership {
                project_file,
                root_schematic,
                instance_paths,
            });
        }
    }

    owners.sort_by(|left, right| left.root_schematic.cmp(&right.root_schematic));
    if owners.len() > 1 {
        return Err(SchematicTargetError::AmbiguousProject {
            target: target.to_path_buf(),
            roots: owners
                .into_iter()
                .map(|owner| owner.root_schematic)
                .collect(),
        });
    }
    Ok(owners.pop())
}

fn collect_sheet_instance_paths(
    current: &std::path::Path,
    target: &std::path::Path,
    suffix: &mut Vec<String>,
    stack: &mut std::collections::HashSet<std::path::PathBuf>,
    depth: usize,
    root_uuid: &str,
    found: &mut Vec<String>,
) {
    if depth > crate::tools::sch_hierarchy::MAX_HIERARCHY_DEPTH {
        return;
    }
    let canonical = canonical_schematic_path(current);
    if !stack.insert(canonical.clone()) {
        return;
    }

    if let Ok(schematic) = konnect_schematic_editor::Schematic::load(current) {
        let directory = current
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        for sheet in &schematic.sheets {
            let child = directory.join(sheet.file());
            suffix.push(sheet.uuid.clone());
            if same_schematic_document(&child, target) {
                let mut path = format!("/{root_uuid}");
                for uuid in suffix.iter() {
                    path.push('/');
                    path.push_str(uuid);
                }
                found.push(path);
            } else if child.is_file() {
                collect_sheet_instance_paths(
                    &child,
                    target,
                    suffix,
                    stack,
                    depth + 1,
                    root_uuid,
                    found,
                );
            }
            suffix.pop();
        }
    }

    stack.remove(&canonical);
}

fn same_schematic_document(left: &std::path::Path, right: &std::path::Path) -> bool {
    canonical_schematic_path(left) == canonical_schematic_path(right)
}

fn canonical_schematic_path(path: &std::path::Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Resolve `sch_path`'s place in its project.
///
/// Falls back to treating the file as its own root only when no parsed project
/// hierarchy owns it. Ambiguous ownership is returned to the caller and must
/// be exposed as a structured refusal.
pub(crate) fn sheet_instance_context(
    sch_path: &std::path::Path,
    sch: &mut konnect_schematic_editor::Schematic,
) -> Result<SheetInstanceContext, SchematicTargetError> {
    let own_root = ensure_root_uuid(sch);
    let standalone = SheetInstanceContext {
        project_name: project_name_for(sch_path),
        instance_path: format!("/{own_root}"),
        instance_paths: vec![format!("/{own_root}")],
        is_child_sheet: false,
    };

    let Some(ownership) = resolve_schematic_ownership(sch_path)? else {
        return Ok(standalone);
    };
    let instance_path = ownership
        .instance_paths
        .first()
        .cloned()
        .unwrap_or_else(|| standalone.instance_path.clone());
    Ok(SheetInstanceContext {
        project_name: ownership
            .project_file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        instance_path,
        instance_paths: ownership.instance_paths,
        is_child_sheet: !same_schematic_document(&ownership.root_schematic, sch_path),
    })
}

/// Prove that every existing placed symbol is keyed to exactly the hierarchy
/// identities observed from the parsed project root.
///
/// A missing, foreign, duplicate, or obsolete path means the file's saved
/// instance metadata is stale. Adding another symbol in that state would
/// produce a document where KiCad resolves different components against
/// different hierarchy instances, so mutation fails closed before the in-memory
/// schematic is changed.
pub(crate) fn validate_sheet_instance_state(
    sch_path: &std::path::Path,
    schematic: &konnect_schematic_editor::Schematic,
    context: &SheetInstanceContext,
) -> Result<(), SchematicTargetError> {
    let mut expected = context
        .instance_paths
        .iter()
        .map(|path| (context.project_name.clone(), path.clone()))
        .collect::<Vec<_>>();
    expected.sort();

    let mut stale_symbols = Vec::new();
    for symbol in &schematic.symbols {
        let mut observed = symbol.instance_paths();
        observed.sort();
        if observed != expected {
            let identity = symbol
                .reference()
                .filter(|reference| !reference.is_empty())
                .unwrap_or(symbol.uuid.as_str());
            let format_paths = |paths: &[(String, String)]| {
                paths
                    .iter()
                    .map(|(project, path)| format!("{project}:{path}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            stale_symbols.push(format!(
                "{identity} observed [{}], expected [{}]",
                format_paths(&observed),
                format_paths(&expected)
            ));
        }
    }

    if stale_symbols.is_empty() {
        Ok(())
    } else {
        Err(SchematicTargetError::StaleTarget {
            target: sch_path.to_path_buf(),
            reason: format!(
                "placed-symbol instance metadata disagrees with project '{}': {}",
                context.project_name,
                stale_symbols.join("; ")
            ),
        })
    }
}

#[cfg(test)]
mod schematic_target_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn write(path: &Path, content: &str) -> PathBuf {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
        path.to_path_buf()
    }

    fn blank(path: &Path) -> PathBuf {
        write(path, &blank_schematic_template())
    }

    fn root_with_child(path: &Path, root_uuid: &str, child: &str, sheet_uuid: &str) -> PathBuf {
        write(
            path,
            &format!(
                r#"(kicad_sch
	(version 20250610)
	(generator "eeschema")
	(uuid "{root_uuid}")
	(paper "A4")
	(lib_symbols)
	(sheet
		(at 20 20)
		(size 40 20)
		(uuid "{sheet_uuid}")
		(property "Sheetname" "Child" (at 20 19.365 0))
		(property "Sheetfile" "{child}" (at 20 40.635 0))
	)
	(sheet_instances (path "/" (page "1")))
)
"#,
            ),
        )
    }

    #[test]
    fn exact_project_root_is_resolved() {
        let directory = tempfile::tempdir().unwrap();
        write(&directory.path().join("control.kicad_pro"), "{}");
        let root = blank(&directory.path().join("control.kicad_sch"));

        let owner = resolve_schematic_ownership(&root).unwrap().unwrap();

        assert_eq!(
            owner.project_file,
            directory.path().join("control.kicad_pro")
        );
        assert_eq!(owner.root_schematic, root);
        assert_eq!(owner.instance_paths.len(), 1);
    }

    #[test]
    fn deep_child_is_proven_through_the_parsed_hierarchy() {
        let directory = tempfile::tempdir().unwrap();
        write(&directory.path().join("control.kicad_pro"), "{}");
        root_with_child(
            &directory.path().join("control.kicad_sch"),
            "root-uuid",
            "sheets/mid.kicad_sch",
            "mid-sheet-uuid",
        );
        root_with_child(
            &directory.path().join("sheets/mid.kicad_sch"),
            "mid-file-uuid",
            "deep/child.kicad_sch",
            "child-sheet-uuid",
        );
        let child = blank(&directory.path().join("sheets/deep/child.kicad_sch"));

        let owner = resolve_schematic_ownership(&child).unwrap().unwrap();

        assert_eq!(
            owner.project_file,
            directory.path().join("control.kicad_pro")
        );
        assert_eq!(
            owner.instance_paths,
            ["/root-uuid/mid-sheet-uuid/child-sheet-uuid"]
        );
    }

    #[test]
    fn loose_sheet_under_an_unrelated_project_stays_loose() {
        let directory = tempfile::tempdir().unwrap();
        write(&directory.path().join("unrelated.kicad_pro"), "{}");
        blank(&directory.path().join("unrelated.kicad_sch"));
        let loose = blank(&directory.path().join("work/loose.kicad_sch"));

        assert_eq!(resolve_schematic_ownership(&loose).unwrap(), None);
        assert_eq!(
            crate::tools::library::project_root_for(&loose).unwrap(),
            loose.parent().map(Path::to_path_buf)
        );
    }

    #[test]
    fn two_structural_owners_are_a_typed_ambiguity() {
        let outer = tempfile::tempdir().unwrap();
        write(&outer.path().join("outer.kicad_pro"), "{}");
        root_with_child(
            &outer.path().join("outer.kicad_sch"),
            "outer-root",
            "nested/child.kicad_sch",
            "outer-path",
        );

        let nested = outer.path().join("nested");
        write(&nested.join("inner.kicad_pro"), "{}");
        root_with_child(
            &nested.join("inner.kicad_sch"),
            "inner-root",
            "child.kicad_sch",
            "inner-path",
        );
        let child = blank(&nested.join("child.kicad_sch"));

        let error = resolve_schematic_ownership(&child).unwrap_err();
        let SchematicTargetError::AmbiguousProject { target, roots } = error else {
            panic!("expected ambiguous project error")
        };
        assert_eq!(target, child);
        assert_eq!(roots.len(), 2);
        let result = SchematicTargetError::AmbiguousProject { target, roots }.into_tool_result();
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("ambiguous_target")
        );
    }

    #[test]
    fn missing_or_unreadable_candidate_root_is_not_ownership_evidence() {
        let directory = tempfile::tempdir().unwrap();
        write(&directory.path().join("missing.kicad_pro"), "{}");
        write(&directory.path().join("broken.kicad_pro"), "{}");
        write(
            &directory.path().join("broken.kicad_sch"),
            "not a schematic",
        );
        let target = blank(&directory.path().join("nested/loose.kicad_sch"));

        assert_eq!(resolve_schematic_ownership(&target).unwrap(), None);
    }
}
