//! McpHandler — receives raw JSON messages from any transport and dispatches
//! to the correct MCP method handler or tool executor.

use super::error::{extract_error_kind, ToolErrorKind};
use super::protocol::*;
use super::server::McpServerState;
use crate::observability::{
    default_calls_log_path, new_call_id, unix_ms, CallObserver, CallRecord, CallStatus,
};
use crate::outcome::OutcomeStatus;
use crate::router::{meta_tools, ToolRouter};
use axum::response::sse::Event;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

/// Clone-able handle to the MCP request handler.
/// Multiple transports (STDIO + HTTP) share the same handler.
#[derive(Clone)]
pub struct McpHandler {
    ctx: Arc<crate::tools::ToolContext>,
    reload: meta_tools::ReloadControl,
    sse_senders: Arc<RwLock<Vec<mpsc::Sender<Event>>>>,
    /// Raw-JSON-line notification sinks for non-SSE transports (stdio). A
    /// server-initiated notification (e.g. tools/list_changed) must reach the
    /// active transport; SSE senders only cover HTTP, so stdio registers here
    /// to receive the same notifications. Without this, notifications are
    /// silently dropped on stdio — the cause of issue #19.
    notif_sinks: Arc<RwLock<Vec<mpsc::Sender<String>>>>,
    observer: CallObserver,
    /// Meta-tools bypass the domain router, so their advertised schemas are
    /// compiled here once and enforced at the same dispatch boundary.
    meta_input_validators: Arc<HashMap<String, Arc<jsonschema::Validator>>>,
}

/// MCP clients known to cache the first `tools/list` and ignore
/// `notifications/tools/list_changed`. For these, any toolset loaded after the
/// handshake is permanently uncallable (#134, #169, #459), so the whole
/// catalogue is loaded before the first listing instead.
///
/// Names are matched case-insensitively as prefixes of `clientInfo.name`.
/// `claude-ai` is what Claude Desktop sends, read from its own MCP log
/// (`%APPDATA%/Claude/logs`) rather than assumed. Add a client here only with
/// the same kind of evidence.
const CLIENTS_THAT_CACHE_TOOL_LIST: &[&str] = &["claude-ai"];

pub(crate) fn client_caches_tool_list(client_name: &str) -> bool {
    let name = client_name.trim().to_ascii_lowercase();
    CLIENTS_THAT_CACHE_TOOL_LIST
        .iter()
        .any(|known| name.starts_with(known))
}

impl McpHandler {
    pub async fn new(config: crate::tools::ServerConfig) -> anyhow::Result<Self> {
        Self::new_with_config_resolution(
            config,
            crate::config_resolution::ConfigResolution::unavailable(),
        )
        .await
    }

    /// As `new`, but recording which configuration file configured this process
    /// so `get_installation_info` can report it (#419). The real server entry
    /// point uses this; `new` keeps its signature for the many callers that do
    /// not resolve a config file and would otherwise have to invent one.
    pub async fn new_with_config_resolution(
        config: crate::tools::ServerConfig,
        config_resolution: crate::config_resolution::ConfigResolution,
    ) -> anyhow::Result<Self> {
        let router = Arc::new(ToolRouter::new());

        // Load only the starter kit at startup so baseline `tools/list` stays small
        // (~2K tokens, not ~23K). The LLM expands on demand via `load_toolset`.
        //
        // `eager_toolsets` opts out of that for clients that cache the initial
        // tool list and ignore `notifications/tools/list_changed` — for those,
        // a tool missing from the first listing is permanently uncallable
        // (#134, #169). Costs ~25K tokens per listing, hence off by default.
        if config.eager_toolsets {
            router.load_all().await;
        } else {
            router.load_starter_kit().await;
        }

        let observer = CallObserver::new(Some(default_calls_log_path()));
        let meta_input_validators = meta_tools::meta_tool_descriptions_for(cfg!(unix))
            .into_iter()
            .map(|tool| {
                let validator =
                    crate::tools::compile_input_validator(&tool.name, &tool.input_schema);
                (tool.name, validator)
            })
            .collect();
        let ctx = Arc::new(
            crate::tools::ToolContext::new_with_observer(config, router, observer.clone())
                .with_config_resolution(config_resolution),
        );

        Ok(McpHandler {
            ctx,
            reload: meta_tools::ReloadControl::default(),
            sse_senders: Arc::new(RwLock::new(Vec::new())),
            notif_sinks: Arc::new(RwLock::new(Vec::new())),
            observer,
            meta_input_validators: Arc::new(meta_input_validators),
        })
    }

    /// Accessor for the `CallObserver` — used by meta-tools `get_recent_calls`
    /// and `server_stats` that live on `ToolContext`.
    pub fn observer(&self) -> &CallObserver {
        &self.observer
    }

    /// Enable the Unix-only in-place reload tool for the standalone executable
    /// when its sole transport is stdio. Embedded, HTTP, and mixed transports
    /// intentionally never call this, so they neither advertise nor dispatch
    /// the operation.
    pub fn enable_stdio_reload(&self) {
        #[cfg(unix)]
        self.reload.enable();
    }

    /// Let `initialize` and `get_installation_info` report installed guidance
    /// drift. Only the standalone binary, which embeds the bundle, calls this;
    /// a second call is ignored.
    pub fn set_guidance_probe(&self, probe: Arc<dyn crate::guidance::GuidanceProbe>) {
        let _ = self.ctx.guidance.set(probe);
    }

    pub fn take_reload_request(&self) -> Option<meta_tools::ReloadPlan> {
        self.reload.take()
    }

    pub async fn register_sse_sender(&self, tx: mpsc::Sender<Event>) {
        self.sse_senders.write().await.push(tx);
    }

    /// Register a raw-JSON-line notification sink (used by the stdio transport).
    /// Each server-initiated notification is delivered here as a serialized
    /// JSON-RPC string, which the transport writes to its output stream.
    pub async fn register_notification_sink(&self, tx: mpsc::Sender<String>) {
        self.notif_sinks.write().await.push(tx);
    }

    /// Process one JSON-RPC message and return an optional response.
    /// Returns `None` for notifications (no response required).
    pub async fn handle_message(&self, msg: Value) -> Option<JsonRpcResponse> {
        // Distinguish request (has "method") from response (has "result"/"error")
        msg.get("method")?;

        let req: JsonRpcRequest = match serde_json::from_value(msg) {
            Ok(r) => r,
            Err(e) => {
                return Some(JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError {
                        code: INVALID_REQUEST,
                        message: format!("Invalid request: {}", e),
                        data: None,
                    },
                ));
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);
        debug!("Handling method: {}", req.method);

        let result = self.dispatch(&req).await;

        match result {
            Ok(None) => None, // notification — no response
            Ok(Some(val)) => Some(JsonRpcResponse::success(id, val)),
            Err(e) => Some(JsonRpcResponse::error(
                id,
                JsonRpcError {
                    code: INTERNAL_ERROR,
                    message: e.to_string(),
                    data: None,
                },
            )),
        }
    }

    /// At `initialize`, look at who is on the other end. A client that caches
    /// its first tool list gets the full catalogue loaded now, before the
    /// `tools/list` that follows the handshake, because for it there is no
    /// later. Every other client keeps the starter kit and the on-demand
    /// loader.
    ///
    /// `eager_toolsets = true` still loads everything at startup for any
    /// client; this only adds the automatic case. Loading is idempotent, so a
    /// client that is both configured eager and detected here loads once.
    async fn adapt_to_client(&self, params: Option<&Value>) {
        let name = params
            .and_then(|p| p.get("clientInfo"))
            .and_then(|c| c.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        tracing::info!(
            "MCP client: {}",
            if name.is_empty() { "(unnamed)" } else { name }
        );
        if client_caches_tool_list(name) {
            tracing::info!(
                "client caches its tool list; loading every toolset before the first tools/list"
            );
            self.ctx.router.load_all().await;
        }
    }

    async fn dispatch(&self, req: &JsonRpcRequest) -> anyhow::Result<Option<Value>> {
        match req.method.as_str() {
            // ── Lifecycle ──────────────────────────────────────────────────
            "initialize" => {
                self.adapt_to_client(req.params.as_ref()).await;
                let notice = self
                    .ctx
                    .guidance
                    .get()
                    .and_then(|probe| probe.take_notice());
                let result = McpServerState::build_initialize_result(notice);
                Ok(Some(serde_json::to_value(result)?))
            }
            "notifications/initialized" => Ok(None),
            "ping" => Ok(Some(json!({}))),

            // ── Tool listing ───────────────────────────────────────────────
            "tools/list" => {
                // Meta-tools (always visible) + all domain tools (pre-loaded at startup)
                let mut tools = meta_tools::meta_tool_descriptions_for(self.reload.is_enabled());
                for def in self.ctx.router.active_tools().await {
                    tools.push(def.to_mcp_description());
                }
                let result = ListToolsResult {
                    tools,
                    next_cursor: None,
                };
                Ok(Some(serde_json::to_value(result)?))
            }

            // ── Tool execution ─────────────────────────────────────────────
            "tools/call" => {
                let params: CallToolParams =
                    serde_json::from_value(req.params.clone().unwrap_or(Value::Null))?;

                let call_result = self.execute_tool(&params).await;
                Ok(Some(serde_json::to_value(call_result)?))
            }

            // ── Unimplemented MCP methods ──────────────────────────────────
            "resources/list" | "resources/read" => Ok(Some(json!({ "resources": [] }))),
            "prompts/list" => Ok(Some(json!({ "prompts": [] }))),

            method => {
                warn!("Unknown method: {}", method);
                Err(anyhow::anyhow!("Method not found: {}", method))
            }
        }
    }

    async fn execute_tool(&self, params: &CallToolParams) -> CallToolResult {
        let args = params.arguments.clone().unwrap_or(json!({}));
        let call_id = new_call_id();
        let started = Instant::now();
        let ts = unix_ms();

        // Pre-compute the owning toolset (if any) once for the call record.
        let toolset = self
            .ctx
            .router
            .find_toolset_for_tool(&params.name)
            .map(str::to_string);

        let args_bytes = serde_json::to_string(&args).map(|s| s.len()).unwrap_or(0);

        info!(
            call_id = %call_id,
            tool = %params.name,
            toolset = toolset.as_deref().unwrap_or("-"),
            "tool_call_start"
        );

        let (result, status, error_kind) = self.dispatch_tool(&params.name, &args).await;

        let dur_ms = started.elapsed().as_millis() as u64;
        let result_bytes = result_content_bytes(&result);

        info!(
            call_id = %call_id,
            tool = %params.name,
            status = %status.as_str(),
            dur_ms = dur_ms,
            "tool_call_end"
        );

        self.observer
            .record(CallRecord {
                call_id,
                ts,
                tool: params.name.clone(),
                toolset,
                dur_ms,
                status,
                error_kind,
                args_bytes,
                result_bytes,
            })
            .await;

        result
    }

    /// Core dispatch: meta-tool → loaded domain tool → actionable error.
    /// Returns the outcome triple so `execute_tool` can record it.
    async fn dispatch_tool(
        &self,
        name: &str,
        args: &Value,
    ) -> (CallToolResult, CallStatus, Option<String>) {
        // Meta-tools always win.
        if let Some(validator) = self.meta_input_validators.get(name) {
            if let Err(error) = validator.validate(args) {
                return schema_argument_error(&error);
            }
        }
        if let Some(result) =
            meta_tools::handle_meta_tool_with_reload(name, args, &self.ctx, &self.reload).await
        {
            if name == "load_toolset" || name == "unload_toolset" {
                self.notify_tools_list_changed().await;
            }
            let status = call_status(&result);
            return (result, status, None);
        }

        // Loaded domain tool? If not and auto-load is enabled (opt-in, off by
        // default -- see `ServerConfig::auto_load_toolsets`), load its toolset
        // and retry in the same call instead of erroring.
        let mut tool_def = self.ctx.router.get_tool(name).await;
        if tool_def.is_none() && self.ctx.config.auto_load_toolsets {
            if let Some(toolset) = self.ctx.router.find_toolset_for_tool(name) {
                self.ctx.router.load(toolset).await;
                self.notify_tools_list_changed().await;
                tool_def = self.ctx.router.get_tool(name).await;
            }
        }

        if let Some(tool_def) = tool_def {
            // Nothing validated `required` before this: the schema is
            // advertised to the client and was never enforced server-side, so
            // a handler reading an absent argument with `unwrap_or` ran with a
            // substituted value and reported success. 25 sites across 18 tools
            // did exactly that (#218); each is now fixed in its own handler,
            // and this stops the next one being written.
            //
            // Presence only. A wrong *type* still reaches the handler, which
            // is where the `require_*` helpers name the field — this is a net
            // beneath them, not a replacement for them.
            if let Some(missing) = first_missing_required(&tool_def.input_schema, args) {
                let reason = "missing".to_string();
                return (
                    CallToolResult::error_kind(
                        ToolErrorKind::InvalidArgument {
                            field: missing.clone(),
                            reason: reason.clone(),
                        },
                        format!("Argument '{missing}' is invalid: {reason}"),
                    ),
                    CallStatus::Error,
                    Some("invalid_argument".to_string()),
                );
            }
            if let Err(error) = tool_def.input_validator.validate(args) {
                return schema_argument_error(&error);
            }
            return match (tool_def.handler)(args, self.ctx.clone()).await {
                Ok(result) => {
                    let status = call_status(&result);
                    // Structured errors carry their own kind in the body; plain-text
                    // errors fall back to "handler_error" via extract_error_kind.
                    let error_kind = extract_error_kind(&result);
                    (result, status, error_kind)
                }
                // A missing argument is the caller's mistake, not the tool
                // failing, and the two call for different reactions: retry with
                // the argument, versus conclude the operation is broken. The
                // `require_*` helpers already draw that line; `get_path` could
                // not, because returning a structured result would change 171
                // call sites — so it carries the distinction in the error chain
                // instead, and this is where it is read back out (#194).
                Err(e) if crate::tools::MissingArgument::field_in(&e).is_some() => {
                    let field = crate::tools::MissingArgument::field_in(&e)
                        .expect("guard matched")
                        .to_string();
                    let reason = "missing or not a string".to_string();
                    (
                        CallToolResult::error_kind(
                            ToolErrorKind::InvalidArgument {
                                field: field.clone(),
                                reason: reason.clone(),
                            },
                            format!("Argument '{field}' is invalid: {reason}"),
                        ),
                        CallStatus::Error,
                        Some("invalid_argument".to_string()),
                    )
                }
                Err(e) if kicad_editor_locked_path(&e).is_some() => {
                    let locked = kicad_editor_locked_path(&e).expect("guard matched");
                    let path = locked.display().to_string();
                    if locked
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("kicad_pcb"))
                    {
                        let reason = kicad_board_lock_reason(&e).to_string();
                        (
                            CallToolResult::error_kind(
                                ToolErrorKind::UnsafeFileFallback {
                                    path: path.clone(),
                                    reason: reason.clone(),
                                },
                                format!(
                                    "Board '{path}' cannot be replaced safely ({reason}): a KiCad editor lock appeared before the committed write, so saved-file authority cannot be proven. Konnect did not modify it. Close or recover Pcbnew, reconcile any unsaved work, and save the authoritative state. Retry only after confirming that no KiCad process owns the board and the lock is gone."
                                ),
                            ),
                            CallStatus::Error,
                            Some("unsafe_file_fallback".to_string()),
                        )
                    } else {
                        (
                            CallToolResult::error_kind(
                                ToolErrorKind::Conflict {
                                    paths: vec![path.clone()],
                                },
                                format!(
                                    "Schematic '{path}' has a KiCad editor lock. Close Eeschema, or resolve a stale lock only after confirming no editor owns the file, then retry."
                                ),
                            ),
                            CallStatus::Error,
                            Some("conflict".to_string()),
                        )
                    }
                }
                Err(e) => {
                    warn!(tool = %name, error = %e, "tool handler returned anyhow::Error");
                    let kind = ToolErrorKind::HandlerError {
                        reason: e.to_string(),
                    };
                    (
                        CallToolResult::error_kind(kind, format!("Tool error: {}", e)),
                        CallStatus::Error,
                        Some("handler_error".to_string()),
                    )
                }
            };
        }

        // Not loaded — try to give an actionable hint.
        match self.ctx.router.find_toolset_for_tool(name) {
            Some(toolset) => {
                let kind = ToolErrorKind::ToolsetNotLoaded {
                    toolset: toolset.to_string(),
                    tool: name.to_string(),
                };
                let msg = format!(
                    "Tool '{}' is in toolset '{}' which is not currently loaded. \
                     Call load_toolset('{}') first, then retry.",
                    name, toolset, toolset
                );
                (
                    CallToolResult::error_kind(kind, msg),
                    CallStatus::NotFound,
                    Some("toolset_not_loaded".to_string()),
                )
            }
            None => {
                let kind = ToolErrorKind::UnknownTool {
                    tool: name.to_string(),
                };
                let msg = format!(
                    "Tool '{}' not found. Use list_toolboxes() to see available toolsets.",
                    name
                );
                (
                    CallToolResult::error_kind(kind, msg),
                    CallStatus::NotFound,
                    Some("unknown_tool".to_string()),
                )
            }
        }
    }

    async fn notify_tools_list_changed(&self) {
        let notification = JsonRpcNotification::new(TOOLS_LIST_CHANGED, None);
        let Ok(json) = serde_json::to_string(&notification) else {
            return;
        };

        // HTTP/SSE clients: wrap the JSON in an SSE event. (Unchanged path.)
        {
            let event = Event::default().data(json.clone());
            let mut senders = self.sse_senders.write().await;
            senders.retain(|tx| tx.try_send(event.clone()).is_ok());
        }

        // stdio (and any other raw-line transport): deliver the JSON directly.
        // try_send is non-blocking, so emitting a notification from inside
        // request handling can never block on a full channel and deadlock the
        // request that triggered it; a dropped sink is pruned like the SSE case.
        {
            let mut sinks = self.notif_sinks.write().await;
            sinks.retain(|tx| tx.try_send(json.clone()).is_ok());
        }
    }
}

fn kicad_editor_locked_path(error: &anyhow::Error) -> Option<&std::path::Path> {
    for cause in error.chain() {
        if let Some(konnect_sexp::SexpError::KiCadEditorLocked { path, .. }) =
            cause.downcast_ref::<konnect_sexp::SexpError>()
        {
            return Some(path);
        }
        if let Some(konnect_schematic_editor::Error::KiCadEditorLocked { path, .. }) =
            cause.downcast_ref::<konnect_schematic_editor::Error>()
        {
            return Some(path);
        }
    }
    None
}

fn kicad_board_lock_reason(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        if let Some(konnect_sexp::SexpError::KiCadEditorLocked {
            inspection_error, ..
        }) = cause.downcast_ref::<konnect_sexp::SexpError>()
        {
            return if inspection_error.is_some() {
                "kicad_lock_unreadable"
            } else {
                "kicad_lock_present"
            };
        }
    }
    "kicad_lock_present"
}

/// Sum of content bytes in a `CallToolResult` — used for observability size
/// accounting. Images are counted by their (already-base64-encoded) data len,
/// which matches what the client sees over the wire.
/// The first name in a schema's `"required"` list that `args` does not carry,
/// in the order the schema lists them.
///
/// A JSON `null` counts as absent: `{"query": null}` is a caller who has not
/// supplied a query, and every `as_str()`/`as_array()` read would treat it the
/// same way.
fn first_missing_required(schema: &Value, args: &Value) -> Option<String> {
    schema
        .get("required")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .find(|key| args.get(*key).map(Value::is_null).unwrap_or(true))
        .map(str::to_string)
}

fn result_content_bytes(result: &CallToolResult) -> usize {
    result
        .content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.len(),
            ToolContent::Image { data, .. } => data.len(),
        })
        .sum()
}

/// A missing *path* argument must reach the caller as `invalid_argument`
/// naming the field, exactly as a missing string argument does.
///
/// These assertions live here rather than beside the tools because the
/// distinction is made here: `get_path` returns an `anyhow::Error` (171 call
/// sites depend on that shape), carrying `MissingArgument` for the dispatch to
/// read back out. A test that calls a handler directly cannot see this — it
/// only sees the `Err` — which is why `library.rs`'s argument tests could not
/// cover path arguments and had to supply them to reach the assertion they
/// wanted (#194).
#[cfg(test)]
mod path_argument_taxonomy_tests {
    use super::*;
    use crate::tools::ServerConfig;

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    fn error_json(result: &CallToolResult) -> Value {
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
    }

    #[tokio::test]
    async fn a_missing_path_argument_is_invalid_argument_naming_the_field() {
        let handler = handler().await;
        // One per family, and each argument name differs, so this cannot pass
        // by a single handler happening to be well behaved. 150 registered
        // tools read a path through `get_path` before anything else, so this
        // is the common shape of a mistyped call, not an edge case.
        for (tool, field) in [
            ("list_symbols_in_library", "library_path"),
            ("get_board_info", "board"),
            ("list_schematic_components", "schematic"),
            ("get_project_info", "path"),
        ] {
            let (result, status, kind) = handler.dispatch_tool(tool, &json!({})).await;
            assert!(result.is_error, "{tool}: a missing path must fail");
            assert!(matches!(status, CallStatus::Error), "{tool}");
            assert_eq!(
                kind.as_deref(),
                Some("invalid_argument"),
                "{tool}: observability must record the argument error, not \
                 handler_error — that is the field a caller filters on"
            );
            let parsed = error_json(&result);
            assert_eq!(parsed["error"]["kind"], "invalid_argument", "{tool}");
            assert_eq!(
                parsed["error"]["field"], field,
                "{tool} must name the path argument it wanted"
            );
        }
    }

    /// The other half of the contract: a path that is *present* but unusable is
    /// the handler trying and failing, and must not be relabelled as the
    /// caller's mistake. Collapsing these two would make "you forgot an
    /// argument" indistinguishable from "that file is not there".
    #[tokio::test]
    async fn a_present_but_unusable_path_is_not_an_argument_error() {
        let handler = handler().await;
        let missing_file = std::env::temp_dir().join("konnect-194-does-not-exist.kicad_pcb");
        let (result, _, kind) = handler
            .dispatch_tool(
                "get_board_info",
                &json!({ "board": missing_file.display().to_string() }),
            )
            .await;
        assert!(result.is_error, "a missing file must still fail");
        assert_ne!(
            kind.as_deref(),
            Some("invalid_argument"),
            "the argument was supplied and well formed; the file is what is wrong"
        );
    }
}

/// Every tool that declares an argument required must refuse the call when it
/// is absent — not substitute a value, do the work, and report success.
///
/// Driven through the dispatch so one table can cover tools from eight
/// different modules. The read-only half of #218: none of these damaged a
/// file, but each returned a confident answer to a question nobody asked.
/// `search_symbols` with no query returned the first 50 symbols across every
/// installed library; `suggest_jlcpcb_alternatives` with neither `value` nor
/// `footprint` returned the five cheapest in-stock parts in the whole JLCPCB
/// database as "alternatives" for a component that was never named.
#[cfg(test)]
mod required_argument_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    #[tokio::test]
    async fn a_missing_required_argument_is_refused_by_name() {
        let handler = handler().await;
        // (tool, args supplying everything except the one under test, field).
        // A path argument is supplied where the handler reads one first, so
        // the assertion is about the field named rather than whichever
        // argument happens to be checked earliest.
        let sch = std::env::temp_dir().join("konnect-218.kicad_sch");
        let s = sch.display().to_string();
        let cases: Vec<(&str, Value, &str)> = vec![
            ("search_symbols", json!({}), "query"),
            ("search_footprints", json!({}), "query"),
            ("search_jlcpcb_parts", json!({}), "query"),
            ("search_templates", json!({}), "query"),
            (
                "suggest_jlcpcb_alternatives",
                json!({ "footprint": "0402" }),
                "value",
            ),
            (
                "suggest_jlcpcb_alternatives",
                json!({ "value": "10k" }),
                "footprint",
            ),
            (
                "batch_delete_schematic_wire",
                json!({ "schematic": s }),
                "uuids",
            ),
            ("batch_add_wire", json!({ "schematic": s }), "wires"),
            ("batch_add_junction", json!({ "schematic": s }), "positions"),
            (
                "batch_delete_no_connect",
                json!({ "schematic": s }),
                "positions",
            ),
            ("batch_rotate_labels", json!({ "schematic": s }), "labels"),
            (
                "bulk_move_schematic_components",
                json!({ "schematic": s, "dx": 1.0, "dy": 1.0 }),
                "references",
            ),
            (
                "batch_get_schematic_pin_locations",
                json!({ "schematic": s }),
                "references",
            ),
        ];

        for (tool, args, field) in cases {
            let (result, _, kind) = handler.dispatch_tool(tool, &args).await;
            assert!(result.is_error, "{tool}: a missing {field} must be refused");
            assert_eq!(
                kind.as_deref(),
                Some("invalid_argument"),
                "{tool}: must record an argument error, not handler_error"
            );
            let text = match result.content.first() {
                Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
                other => panic!("{tool}: expected text, got {other:?}"),
            };
            let parsed: Value =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}"));
            assert_eq!(parsed["error"]["kind"], "invalid_argument", "{tool}");
            assert_eq!(
                parsed["error"]["field"], field,
                "{tool} must name the argument it wanted: {text}"
            );
        }
    }

    /// The advertised `side` enum is enforced at dispatch, so widening it to
    /// four edges is what makes `top` and `bottom` reachable at all. Every
    /// other test of this feature calls the handlers directly and never passes
    /// through this validator, so a schema narrowed back to two edges would
    /// leave the whole feature unreachable with the entire suite still green.
    /// The `middle` case is this test's own positive control: if it stops
    /// being refused, the validator is not running and the rest proves nothing.
    #[tokio::test]
    async fn the_dispatch_accepts_all_four_sheet_pin_edges_and_refuses_a_fifth() {
        let handler = handler().await;
        let schematic_path = std::env::temp_dir().join("konnect-sides.kicad_sch");
        let schematic_path_string = schematic_path.display().to_string();

        // Each tool gets only its own arguments: the records are closed, so a
        // stray key is refused before `side` is ever looked at.
        let args_for = |tool: &str, side: &str| match tool {
            "add_sheet_pin" => {
                json!({ "schematic": schematic_path_string, "sheet_name": "A", "pin_name": "P",
                                       "pin_type": "input", "x": 50.0, "y": 60.0, "side": side })
            }
            "edit_sheet_pin" => {
                json!({ "schematic": schematic_path_string, "sheet_name": "A", "pin_name": "P",
                                        "side": side })
            }
            _ => json!({ "schematic": schematic_path_string, "sheet_name": "A", "side": side }),
        };

        for tool in ["add_sheet_pin", "edit_sheet_pin", "import_sheet_pins"] {
            for side in ["right", "left", "top", "bottom"] {
                let args = args_for(tool, side);
                let (result, _, _) = handler.dispatch_tool(tool, &args).await;
                if result.is_error {
                    let text = match result.content.first() {
                        Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
                        other => panic!("{tool}/{side}: expected text, got {other:?}"),
                    };
                    let parsed: Value = serde_json::from_str(&text)
                        .unwrap_or_else(|e| panic!("{tool}/{side}: {e}: {text}"));
                    assert_ne!(
                        parsed["error"]["field"], "side",
                        "{tool}: the schema refused the advertised edge {side}: {text}"
                    );
                }
            }

            let args = args_for(tool, "middle");
            let (result, _, _) = handler.dispatch_tool(tool, &args).await;
            assert!(
                result.is_error,
                "{tool}: control — an edge that does not exist must be refused"
            );
            let text = match result.content.first() {
                Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
                other => panic!("{tool}: expected text, got {other:?}"),
            };
            let parsed: Value =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}"));
            assert_eq!(
                parsed["error"]["field"], "side",
                "{tool}: control — the refusal must name `side`, or this test is inert: {text}"
            );
        }
    }

    /// An explicitly empty list is a coherent request — "operate on nothing" —
    /// and must stay distinguishable from forgetting to say what to operate
    /// on. Refusing both would trade one conflated pair for another.
    #[tokio::test]
    async fn an_explicitly_empty_list_is_not_an_argument_error() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let sch = dir.path().join("empty.kicad_sch");
        std::fs::write(
            &sch,
            "(kicad_sch\n\t(version 20250114)\n\t(generator \"eeschema\")\n\t\
             (uuid \"r\")\n\t(paper \"A4\")\n\t(lib_symbols)\n)\n",
        )
        .unwrap();

        let (_, _, kind) = handler
            .dispatch_tool(
                "batch_delete_schematic_wire",
                &json!({ "schematic": sch.display().to_string(), "uuids": [] }),
            )
            .await;
        assert_ne!(
            kind.as_deref(),
            Some("invalid_argument"),
            "an empty uuids list is a request to delete nothing, not a mistake"
        );
    }

    #[tokio::test]
    async fn a_kicad_schematic_lock_is_a_typed_conflict() {
        let handler = handler().await;
        let directory = tempfile::tempdir().unwrap();
        let schematic = directory.path().join("locked.kicad_sch");
        let lock = directory.path().join("~locked.kicad_sch.lck");
        let source = "(kicad_sch\n\t(version 20250114)\n\t(generator \"eeschema\")\n\t\
                      (uuid \"r\")\n\t(paper \"A4\")\n\t(lib_symbols)\n)\n";
        std::fs::write(&schematic, source).unwrap();
        std::fs::write(
            &lock,
            r#"{"username":"konnect-test","hostname":"test-host"}"#,
        )
        .unwrap();

        let (result, status, kind) = handler
            .dispatch_tool(
                "add_wire",
                &json!({
                    "schematic": schematic.display().to_string(),
                    "x1": 10.0,
                    "y1": 10.0,
                    "x2": 20.0,
                    "y2": 10.0
                }),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(status, CallStatus::Error);
        assert_eq!(kind.as_deref(), Some("conflict"));
        let text = match result.content.first() {
            Some(ToolContent::Text { text }) => text,
            other => panic!("expected text, got {other:?}"),
        };
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["error"]["kind"], "conflict");
        assert_eq!(
            body["error"]["paths"],
            json!([schematic.display().to_string()])
        );
        assert_eq!(std::fs::read_to_string(schematic).unwrap(), source);
        assert!(lock.exists());
    }

    #[tokio::test]
    async fn a_kicad_board_lock_is_a_typed_unsafe_file_fallback() {
        let handler = handler().await;
        let directory = tempfile::tempdir().unwrap();
        let board = directory.path().join("locked.kicad_pcb");
        let lock = directory.path().join("~locked.kicad_pcb.lck");
        let source = "(kicad_pcb (version 20240108) (generator pcbnew))\n";
        std::fs::write(&board, source).unwrap();
        std::fs::write(&lock, "lock ownership cannot be inferred").unwrap();

        let (result, status, kind) = handler
            .dispatch_tool(
                "add_mounting_hole",
                &json!({
                    "board": board.display().to_string(),
                    "x": 10.0,
                    "y": 10.0,
                    "reference": "H1"
                }),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(status, CallStatus::Error);
        assert_eq!(kind.as_deref(), Some("unsafe_file_fallback"));
        let text = match result.content.first() {
            Some(ToolContent::Text { text }) => text,
            other => panic!("expected text, got {other:?}"),
        };
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["error"]["kind"], "unsafe_file_fallback");
        assert_eq!(body["error"]["reason"], "kicad_lock_present");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), source);
        assert!(lock.exists());
    }
}

/// Every registered tool refuses a call that omits its required arguments.
///
/// Exhaustive rather than a sample, and safe to be exhaustive *because of* the
/// check it tests: with every required argument absent the dispatch refuses
/// before the handler runs, so no tool touches a file, a board, or the
/// network. Removing the check would make this both fail and unsafe, which is
/// the right relationship between a guard and its test.
///
/// The per-handler `require_*` calls remain the primary defence — they name
/// the field for a wrong *type*, which presence-checking cannot see. This
/// asserts the floor beneath them: a tool added tomorrow that reads a required
/// argument with `unwrap_or` still cannot run with a substituted value (#218).
#[cfg(test)]
mod every_tool_enforces_its_required_arguments {
    use super::*;
    use crate::router::registry;
    use crate::tools::ServerConfig;

    #[tokio::test]
    async fn calling_any_tool_with_no_arguments_names_its_first_required_one() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");

        let mut checked = 0usize;
        let mut wrong = Vec::new();

        for toolset in registry::ALL_TOOLSETS {
            for def in registry::tools_for(toolset.name).unwrap_or_default() {
                let Some(first) = def.input_schema["required"]
                    .as_array()
                    .and_then(|r| r.first())
                    .and_then(|v| v.as_str())
                else {
                    continue; // no required arguments to omit
                };

                let (result, _, kind) = handler.dispatch_tool(def.name, &json!({})).await;
                checked += 1;

                if !result.is_error || kind.as_deref() != Some("invalid_argument") {
                    wrong.push(format!(
                        "{}: expected invalid_argument naming '{first}', got kind={:?} \
                         is_error={}",
                        def.name, kind, result.is_error
                    ));
                    continue;
                }
                let text = match result.content.first() {
                    Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
                    other => {
                        wrong.push(format!("{}: expected text, got {other:?}", def.name));
                        continue;
                    }
                };
                match serde_json::from_str::<Value>(&text) {
                    Ok(parsed) if parsed["error"]["field"] == json!(first) => {}
                    Ok(parsed) => wrong.push(format!(
                        "{}: named '{}', schema lists '{first}' first",
                        def.name, parsed["error"]["field"]
                    )),
                    Err(e) => wrong.push(format!("{}: {e}: {text}", def.name)),
                }
            }
        }

        assert!(
            checked > 150,
            "expected to cover most of the catalogue, only checked {checked}"
        );
        assert!(
            wrong.is_empty(),
            "{} of {checked} tools do not refuse a call with no arguments:\n  {}",
            wrong.len(),
            wrong.join("\n  ")
        );
    }
}

#[cfg(test)]
mod reliability_contract_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    #[tokio::test]
    async fn reliability_contract_outcome_survives_the_served_dispatch() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let dir = tempfile::tempdir().unwrap();
        let schematic = dir.path().join("review.kicad_sch");
        std::fs::write(
            &schematic,
            "(kicad_sch\n\t(version 20250114)\n\t(generator \"eeschema\")\n\t\
             (uuid \"00000000-0000-0000-0000-000000000001\")\n\t(paper \"A4\")\n\t\
             (lib_symbols)\n)\n",
        )
        .unwrap();

        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "run_design_review",
                    "arguments": {"schematic": schematic.display().to_string()}
                }
            }))
            .await
            .expect("request returns a response");
        let result = response.result.expect("successful JSON-RPC response");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns JSON text");
        let body: Value = serde_json::from_str(text).expect("tool body is JSON");
        assert!(
            ["complete", "partial", "failed", "uncertain"]
                .contains(&body["outcome"]["status"].as_str().unwrap_or("")),
            "served tools/call lost the shared outcome: {body}"
        );
        assert_eq!(body["outcome"]["target"], schematic.display().to_string());
    }
}

#[cfg(test)]
mod no_connect_carry_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    const CARRY: &str = include_str!("../../tests/fixtures/no_connect_carry_kicad10.kicad_sch");
    const R2_MARKER: &str = "1a251b9a-30be-43fa-bf5f-04706ed82788";

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    async fn call(handler: &McpHandler, name: &str, arguments: Value) -> Value {
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }))
            .await
            .expect("request returns a response");
        response.result.expect("successful JSON-RPC response")
    }

    fn text_body(result: &Value) -> Value {
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns JSON text");
        serde_json::from_str(text).expect("tool body is JSON")
    }

    /// `no_connects_moved` and `no_connects_moved_count` are public response
    /// fields (#626); prove they survive the served `tools/call` boundary with
    /// the marker actually written to the file.
    #[tokio::test]
    async fn a_move_reports_the_marker_it_carried_across_the_served_dispatch() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let schematic = dir.path().join("carry.kicad_sch");
        std::fs::write(&schematic, CARRY).unwrap();

        let result = call(
            &handler,
            "move_schematic_component",
            json!({
                "schematic": schematic.display().to_string(),
                "reference": "R2",
                "x": 154.94,
                "y": 163.83
            }),
        )
        .await;
        assert_ne!(result["isError"], json!(true), "{result}");
        let body = text_body(&result);
        assert_eq!(body["no_connects_moved_count"], 1, "{body}");
        assert_eq!(body["no_connects_moved"][0]["uuid"], R2_MARKER, "{body}");
        assert_eq!(body["junctions_added_count"], 0, "{body}");
        assert!(std::fs::read_to_string(&schematic)
            .unwrap()
            .contains("(at 154.94 160.02)"));
    }

    /// The refusal has to survive the same boundary, structured and with the
    /// file untouched — a caller that only sees `isError` still must not be
    /// told a placement change happened.
    #[tokio::test]
    async fn an_unfollowable_marker_refuses_across_the_served_dispatch() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let schematic = dir.path().join("carry.kicad_sch");
        std::fs::write(&schematic, CARRY).unwrap();

        let result = call(
            &handler,
            "move_schematic_component",
            json!({
                "schematic": schematic.display().to_string(),
                "reference": "R3",
                "x": 101.6,
                "y": 101.6
            }),
        )
        .await;
        assert_eq!(result["isError"], json!(true), "{result}");
        let body = text_body(&result);
        assert_eq!(body["error"]["kind"], "ambiguous_target", "{body}");
        assert_eq!(std::fs::read_to_string(&schematic).unwrap(), CARRY);
    }
}

#[cfg(test)]
mod active_layer_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    #[tokio::test]
    async fn set_active_layer_refusal_survives_served_dispatch_without_writing() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("safe.kicad_pcb");
        let original = b"(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n\t(setup\n\t\t(pad_to_mask_clearance 0)\n\t)\n)\n";
        std::fs::write(&board, original).unwrap();

        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 610,
                "method": "tools/call",
                "params": {
                    "name": "set_active_layer",
                    "arguments": {
                        "board": board.display().to_string(),
                        "layer": "B.Cu"
                    }
                }
            }))
            .await
            .expect("request returns a response");
        let result = response.result.expect("JSON-RPC tool result");
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns JSON text");
        let body: Value = serde_json::from_str(text).expect("tool body is JSON");

        assert_eq!(body["error"]["kind"], "unsupported_capability");
        assert_eq!(body["error"]["capability"], "set_active_layer");
        assert_eq!(std::fs::read(&board).unwrap(), original);
    }
}

#[cfg(test)]
mod current_board_template_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    #[tokio::test]
    async fn create_project_board_is_refused_by_add_net_without_writing() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let dir = tempfile::tempdir().unwrap();

        let create = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 631,
                "method": "tools/call",
                "params": {
                    "name": "create_project",
                    "arguments": {
                        "path": dir.path().display().to_string(),
                        "name": "current"
                    }
                }
            }))
            .await
            .expect("create_project returns a response");
        let create_result = create.result.expect("JSON-RPC tool result");
        assert_ne!(create_result["isError"], true, "{create_result}");

        let board = dir.path().join("current.kicad_pcb");
        let original = std::fs::read(&board).expect("created board");
        let original_text = std::str::from_utf8(&original).unwrap();
        assert!(original_text.contains("(version 20260206)"));
        assert!(!original_text.contains("\n\t(net "));

        let add_net = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 632,
                "method": "tools/call",
                "params": {
                    "name": "add_net",
                    "arguments": {
                        "board": board.display().to_string(),
                        "net_name": "PROBE_NET"
                    }
                }
            }))
            .await
            .expect("add_net returns a response");
        let add_net_result = add_net.result.expect("JSON-RPC tool result");
        assert_eq!(add_net_result["isError"], true, "{add_net_result}");
        let error_body: Value = serde_json::from_str(
            add_net_result["content"][0]["text"]
                .as_str()
                .expect("tool returns JSON text"),
        )
        .expect("tool body is JSON");
        assert_eq!(error_body["error"]["kind"], "unsupported_capability");
        assert!(
            error_body["message"]
                .as_str()
                .is_some_and(|text| text.contains("KiCad 10")),
            "{error_body}"
        );
        assert_eq!(
            std::fs::read(&board).unwrap(),
            original,
            "the refused operation must not change the generated board"
        );
    }
}

#[cfg(test)]
mod legacy_add_net_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::path::Path;

    fn body(response: JsonRpcResponse) -> Value {
        let result = response.result.expect("JSON-RPC tool result");
        assert_ne!(result["isError"], true, "{result}");
        serde_json::from_str(
            result["content"][0]["text"]
                .as_str()
                .expect("tool returns JSON text"),
        )
        .expect("tool body is JSON")
    }

    async fn call_add_net(handler: &McpHandler, board: &Path, name: &str) -> Value {
        body(
            handler
                .handle_message(json!({
                    "jsonrpc": "2.0",
                    "id": 631,
                    "method": "tools/call",
                    "params": {
                        "name": "add_net",
                        "arguments": {
                            "board": board.display().to_string(),
                            "net_name": name
                        }
                    }
                }))
                .await
                .expect("add_net returns a response"),
        )
    }

    #[tokio::test]
    async fn served_add_net_is_idempotent_and_reaches_the_board_logic() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("legacy.kicad_pcb");
        let original = b"(kicad_pcb\n\t(version 20241229)\n\t(net 0 \"\")\n\t(net 7 \"GND\")\n)\n";
        std::fs::write(&board, original).unwrap();

        let existing = call_add_net(&handler, &board, "GND").await;
        assert_eq!(existing["net_id"], 7);
        assert_eq!(existing["created"], false);
        assert_eq!(std::fs::read(&board).unwrap(), original);

        let created = call_add_net(&handler, &board, "PROBE_NET").await;
        assert_eq!(created["net_id"], 8);
        assert_eq!(created["created"], true);
        let after_create = std::fs::read(&board).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after_create)
                .matches("\"PROBE_NET\"")
                .count(),
            1
        );

        let repeated = call_add_net(&handler, &board, "PROBE_NET").await;
        assert_eq!(repeated["net_id"], 8);
        assert_eq!(repeated["created"], false);
        assert_eq!(std::fs::read(&board).unwrap(), after_create);
    }
}

#[cfg(test)]
mod annotate_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    /// `annotate_schematic`'s `outcome`, `assigned` and `unresolved` are
    /// public response fields (#454); prove they survive the served
    /// `tools/call` boundary on the KiCad-authored duplicate fixture.
    #[tokio::test]
    async fn annotate_outcome_and_unresolved_survive_the_served_dispatch() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let dir = tempfile::tempdir().unwrap();
        let schematic = dir.path().join("annotate.kicad_sch");
        std::fs::write(
            &schematic,
            include_str!("../../tests/fixtures/annotate_duplicates.kicad_sch"),
        )
        .unwrap();

        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "annotate_schematic",
                    "arguments": {"schematic": schematic.display().to_string()}
                }
            }))
            .await
            .expect("request returns a response");
        let result = response.result.expect("successful JSON-RPC response");
        assert_ne!(result["isError"], json!(true), "{result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns JSON text");
        let body: Value = serde_json::from_str(text).expect("tool body is JSON");
        assert_eq!(body["outcome"]["status"], "partial", "{body}");
        assert_eq!(body["assigned"].as_array().map(Vec::len), Some(2), "{body}");
        assert_eq!(body["unresolved"][0]["reference"], "R1", "{body}");
        assert_eq!(body["written"], true, "{body}");
        // The fixture names one project and no .kicad_pro sits beside it, so
        // that project is selected and reported.
        assert_eq!(body["project"], "adj", "{body}");
        assert_eq!(body["assigned"][0]["project"], "adj", "{body}");
        assert_eq!(body["unresolved"][0]["project"], "adj", "{body}");
    }
}

#[cfg(test)]
mod hierarchy_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    /// The canonical hierarchy footer is a file-format compatibility guard,
    /// so prove it is present after the deployed `tools/call` path rather than
    /// testing only the private handler (#643).
    #[tokio::test]
    async fn add_hierarchical_sheet_writes_kicad10_root_footer_through_dispatch() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let dir = tempfile::tempdir().unwrap();
        let schematic = dir.path().join("root.kicad_sch");
        std::fs::write(&schematic, crate::tools::blank_schematic_template()).unwrap();

        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 643,
                "method": "tools/call",
                "params": {
                    "name": "add_hierarchical_sheet",
                    "arguments": {
                        "schematic": schematic.display().to_string(),
                        "sheet_file": "power.kicad_sch",
                        "sheet_name": "Power"
                    }
                }
            }))
            .await
            .expect("request returns a response");
        let result = response.result.expect("successful JSON-RPC response");
        assert_ne!(result["isError"], json!(true), "{result}");

        let source = std::fs::read_to_string(&schematic).unwrap();
        let tree = konnect_sexp::parse_sexp(&source).unwrap();
        let root_path = tree
            .find("sheet_instances")
            .and_then(|instances| instances.find("path"))
            .expect("served add writes the root sheet instance");
        assert_eq!(root_path.get(1).and_then(|value| value.as_str()), Some("/"));
        assert_eq!(tree.find_str("embedded_fonts"), Some("no"));
        assert!(source.contains("(exclude_from_sim no)"));
        assert!(source.contains("(do_not_autoplace no)"));
    }
}

#[cfg(test)]
mod rotate_junction_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    /// `rotate_schematic_component`'s `junctions_added_count` and
    /// `junctions_pruned_count` are public response fields (#615); prove they
    /// survive the served `tools/call` boundary on the KiCad-authored fixture,
    /// with the dot actually written to the file.
    #[tokio::test]
    async fn rotate_reports_the_junction_it_added_across_the_served_dispatch() {
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let dir = tempfile::tempdir().unwrap();
        let schematic = dir.path().join("rotate.kicad_sch");
        std::fs::write(
            &schematic,
            include_str!("../../tests/fixtures/rotate_junctions_kicad10.kicad_sch"),
        )
        .unwrap();

        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "rotate_schematic_component",
                    "arguments": {
                        "schematic": schematic.display().to_string(),
                        "reference": "R2",
                        "rotation": 90
                    }
                }
            }))
            .await
            .expect("request returns a response");
        let result = response.result.expect("successful JSON-RPC response");
        assert_ne!(result["isError"], json!(true), "{result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns JSON text");
        let body: Value = serde_json::from_str(text).expect("tool body is JSON");
        assert_eq!(body["junctions_added_count"], 1, "{body}");
        assert_eq!(body["junctions_pruned_count"], 0, "{body}");
        // R2 pin 1 turns onto the NETB wire mid-span; without the dot KiCad
        // leaves it on unconnected-(R2-Pad1).
        assert!(std::fs::read_to_string(&schematic)
            .unwrap()
            .contains("(at 154.94 160.02)"));
    }
}

#[cfg(test)]
mod ipc_failure_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    /// `ipc_failure` is a public response field (#532), so it has to survive
    /// the served `tools/call` boundary, not only the handler functions the
    /// tool tests call directly. The endpoint is a socket nothing listens on.
    #[tokio::test]
    async fn ipc_failure_kind_and_message_survive_the_served_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: format!("ipc://{}", dir.path().join("no-kicad-here.sock").display()),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");

        for (tool, arguments) in [
            ("check_kicad_ui", json!({"timeout_seconds": 60})),
            ("open_project", json!({})),
        ] {
            let response = handler
                .handle_message(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": tool, "arguments": arguments}
                }))
                .await
                .expect("request returns a response");
            let result = response.result.expect("successful JSON-RPC response");
            assert_ne!(result["isError"], json!(true), "{tool}: {result}");
            let text = result["content"][0]["text"]
                .as_str()
                .expect("tool returns JSON text");
            let body: Value = serde_json::from_str(text).expect("tool body is JSON");
            assert_eq!(body["ipc_failure"]["kind"], "no_listener", "{tool}: {body}");
            assert!(
                body["ipc_failure"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("Nothing is listening there")),
                "{tool}: {body}"
            );
        }
    }
}

/// Turn a JSON Schema failure into Konnect's stable structured argument shape.
/// Prefer a concrete nested leaf over an applicator (`oneOf`/`anyOf`) wrapper,
/// and include an unexpected property's own name rather than only its parent.
fn schema_validation_error(error: &jsonschema::ValidationError<'_>) -> (String, String) {
    use jsonschema::error::ValidationErrorKind;

    fn specific<'a>(
        error: &'a jsonschema::ValidationError<'a>,
    ) -> &'a jsonschema::ValidationError<'a> {
        let alternatives = match error.kind() {
            ValidationErrorKind::OneOfNotValid { context }
            | ValidationErrorKind::AnyOf { context } => Some(context),
            _ => None,
        };
        let Some(alternatives) = alternatives else {
            return error;
        };

        alternatives
            .iter()
            .min_by_key(|errors| errors.len())
            .and_then(|errors| {
                errors
                    .iter()
                    .find(|candidate| {
                        matches!(
                            candidate.kind(),
                            ValidationErrorKind::AdditionalProperties { .. }
                                | ValidationErrorKind::Required { .. }
                                | ValidationErrorKind::Type { .. }
                        )
                    })
                    .or_else(|| errors.first())
            })
            .map(specific)
            .unwrap_or(error)
    }

    fn field_path(pointer: &str) -> String {
        let mut field = String::new();
        for raw in pointer.split('/').skip(1) {
            let segment = raw.replace("~1", "/").replace("~0", "~");
            if segment.chars().all(|ch| ch.is_ascii_digit()) {
                field.push('[');
                field.push_str(&segment);
                field.push(']');
            } else {
                if !field.is_empty() {
                    field.push('.');
                }
                field.push_str(&segment);
            }
        }
        field
    }

    let error = specific(error);
    let mut field = field_path(&error.instance_path().to_string());
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            if let Some(property) = unexpected.first() {
                if !field.is_empty() {
                    field.push('.');
                }
                field.push_str(property);
            }
        }
        ValidationErrorKind::Required { property } => {
            if let Some(property) = property.as_str() {
                if !field.is_empty() {
                    field.push('.');
                }
                field.push_str(property);
            }
        }
        _ => {}
    }
    if field.is_empty() {
        field = "arguments".to_string();
    }
    (field, error.to_string())
}

fn schema_argument_error(
    error: &jsonschema::ValidationError<'_>,
) -> (CallToolResult, CallStatus, Option<String>) {
    let (field, reason) = schema_validation_error(error);
    (
        CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: field.clone(),
                reason: reason.clone(),
            },
            format!("Argument '{field}' is invalid: {reason}"),
        ),
        CallStatus::Error,
        Some("invalid_argument".to_string()),
    )
}

fn call_status(result: &CallToolResult) -> CallStatus {
    match crate::outcome::status(result) {
        Some(OutcomeStatus::Complete) => CallStatus::Ok,
        Some(OutcomeStatus::Partial) => CallStatus::Partial,
        Some(OutcomeStatus::Failed) => CallStatus::Error,
        Some(OutcomeStatus::Uncertain) => CallStatus::Uncertain,
        None if result.is_error => CallStatus::Error,
        None => CallStatus::Ok,
    }
}

/// Schema-declared input rules are a server contract, not client guidance.
/// These cases exercise the served dispatch so a malformed present value is
/// refused before a handler can substitute a default or touch a design file.
#[cfg(test)]
mod schema_validation_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    fn error_json(result: &CallToolResult) -> Value {
        let text = match result.content.first() {
            Some(ToolContent::Text { text }) => text,
            other => panic!("expected structured text error, got {other:?}"),
        };
        serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
    }

    #[test]
    fn observer_status_distinguishes_shared_partial_and_uncertain_outcomes() {
        let partial = crate::outcome::attach(
            CallToolResult::json(&json!({"placed_count": 1})),
            crate::outcome::summary(
                OutcomeStatus::Partial,
                "test.kicad_sch",
                "saved_file_readback",
                2,
                1,
                1,
                None,
            ),
        );
        assert_eq!(call_status(&partial), CallStatus::Partial);

        let uncertain = crate::outcome::attach(
            CallToolResult::error("{}"),
            crate::outcome::summary(
                OutcomeStatus::Uncertain,
                "test.kicad_sch",
                "saved_file_readback",
                1,
                0,
                1,
                None,
            ),
        );
        assert_eq!(call_status(&uncertain), CallStatus::Uncertain);
    }

    async fn assert_invalid_field(handler: &McpHandler, tool: &str, args: Value, field: &str) {
        let (result, status, kind) = handler.dispatch_tool(tool, &args).await;
        assert!(result.is_error, "{tool}: malformed input must be refused");
        assert_eq!(status, CallStatus::Error, "{tool}");
        assert_eq!(kind.as_deref(), Some("invalid_argument"), "{tool}");
        let body = error_json(&result);
        assert_eq!(body["error"]["field"], field, "{tool}: {body}");
    }

    #[tokio::test]
    async fn json_rpc_tools_call_returns_the_structured_schema_error() {
        let handler = handler().await;
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 17,
                "method": "tools/call",
                "params": {
                    "name": "add_schematic_component",
                    "arguments": {
                        "schematic": "does-not-exist.kicad_sch",
                        "lib_id": "Amplifier_Operational:LM2904",
                        "x": 10.0,
                        "y": 10.0,
                        "unit": 2.7
                    }
                }
            }))
            .await
            .expect("requests receive a JSON-RPC response");

        assert!(response.error.is_none(), "tool errors are MCP results");
        let result: CallToolResult =
            serde_json::from_value(response.result.expect("tools/call returns a result"))
                .expect("result uses the advertised MCP shape");
        assert!(result.is_error);
        let body = error_json(&result);
        assert_eq!(body["error"]["kind"], "invalid_argument");
        assert_eq!(body["error"]["field"], "unit");
    }

    #[tokio::test]
    async fn wrong_typed_and_fractional_integer_options_are_refused_by_field() {
        let handler = handler().await;
        let schematic = "does-not-exist.kicad_sch";

        for unit in [json!("two"), json!(2.7)] {
            for (tool, args, field) in [
                (
                    "add_schematic_component",
                    json!({
                        "schematic": schematic,
                        "lib_id": "Amplifier_Operational:LM2904",
                        "x": 10.0,
                        "y": 10.0,
                        "unit": unit
                    }),
                    "unit",
                ),
                (
                    "replace_component",
                    json!({
                        "schematic": schematic,
                        "reference": "U1",
                        "new_lib_id": "Amplifier_Operational:LM2904",
                        "unit": unit
                    }),
                    "unit",
                ),
                (
                    "batch_place_components",
                    json!({
                        "schematic": schematic,
                        "components": [{
                            "lib_id": "Amplifier_Operational:LM2904",
                            "x": 10.0,
                            "y": 10.0,
                            "unit": unit
                        }]
                    }),
                    "components[0].unit",
                ),
            ] {
                assert_invalid_field(&handler, tool, args, field).await;
            }
        }
    }

    #[tokio::test]
    async fn integral_json_number_satisfies_an_integer_schema() {
        let handler = handler().await;
        let (result, _, kind) = handler
            .dispatch_tool(
                "add_schematic_component",
                &json!({
                    "schematic": "does-not-exist.kicad_sch",
                    "lib_id": "Amplifier_Operational:LM2904",
                    "x": 10.0,
                    "y": 10.0,
                    "unit": 2.0
                }),
            )
            .await;

        if kind.as_deref() == Some("invalid_argument") {
            let body = error_json(&result);
            assert_ne!(body["error"]["field"], "unit", "integral 2.0 is unit 2");
        }
    }

    #[tokio::test]
    async fn declared_bounds_are_refused_before_a_handler_runs() {
        let handler = handler().await;
        assert_invalid_field(
            &handler,
            "add_zone",
            json!({
                "board": "does-not-exist.kicad_pcb",
                "net_name": "GND",
                "layer": "F.Cu",
                "points": [
                    {"x": 0.0, "y": 0.0},
                    {"x": 10.0, "y": 0.0},
                    {"x": 10.0, "y": 10.0}
                ],
                "priority": -5
            }),
            "priority",
        )
        .await;
    }

    #[tokio::test]
    async fn meta_tools_enforce_the_schema_they_advertise() {
        let handler = handler().await;
        assert_invalid_field(
            &handler,
            "get_recent_calls",
            json!({"limit": "many"}),
            "limit",
        )
        .await;
    }

    #[tokio::test]
    async fn a_nested_unknown_key_is_refused_without_changing_the_file() {
        let handler = handler().await;
        let directory = tempfile::tempdir().unwrap();
        let footprint = directory.path().join("socket.kicad_mod");
        let source = include_str!("../../tests/fixtures/socket_kicad10.kicad_mod");
        std::fs::write(&footprint, source).unwrap();

        assert_invalid_field(
            &handler,
            "set_footprint_graphics",
            json!({
                "footprint_path": footprint.display().to_string(),
                "selector": {"layer": "F.SilkS"},
                "mode": "append",
                "graphics": [{
                    "type": "line",
                    "start": {"x": 0.0, "y": 0.0},
                    "end": {"x": 1.0, "y": 0.0},
                    "stroke_width_mm": 0.2,
                    "colour": "red"
                }]
            }),
            "graphics[0].colour",
        )
        .await;

        assert_eq!(
            std::fs::read_to_string(&footprint).unwrap(),
            source,
            "dispatch refusal must happen before the footprint writer runs"
        );
    }

    #[tokio::test]
    async fn a_pad_zone_connect_outside_the_enum_is_refused_without_changing_the_file() {
        let handler = handler().await;
        let directory = tempfile::tempdir().unwrap();
        let footprint = directory.path().join("MountingHole.kicad_mod");
        let source = include_str!("../../tests/fixtures/pad_zone_connect_kicad10.kicad_mod");
        std::fs::write(&footprint, source).unwrap();

        assert_invalid_field(
            &handler,
            "edit_footprint_pad",
            json!({
                "footprint_path": footprint.display().to_string(),
                "pad_number": "1",
                "zone_connect": "thermal_pth"
            }),
            "zone_connect",
        )
        .await;
        assert_eq!(std::fs::read_to_string(&footprint).unwrap(), source);

        let (result, _, _) = handler
            .dispatch_tool(
                "edit_footprint_pad",
                &json!({
                    "footprint_path": footprint.display().to_string(),
                    "pad_number": "1",
                    "match_all": true,
                    "zone_connect": "thermal"
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        let written = std::fs::read_to_string(&footprint).unwrap();
        assert_eq!(written.matches("(zone_connect 1)").count(), 3);
        assert!(!written.contains("(zone_connect 2)"));
    }

    #[tokio::test]
    async fn an_omitted_pad_zone_connect_is_left_alone_and_null_is_refused() {
        let handler = handler().await;
        let directory = tempfile::tempdir().unwrap();
        let footprint = directory.path().join("MountingHole.kicad_mod");
        let source = include_str!("../../tests/fixtures/pad_zone_connect_kicad10.kicad_mod");
        std::fs::write(&footprint, source).unwrap();

        assert_invalid_field(
            &handler,
            "edit_footprint_pad",
            json!({
                "footprint_path": footprint.display().to_string(),
                "pad_number": "1",
                "match_all": true,
                "zone_connect": null
            }),
            "zone_connect",
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(&footprint).unwrap(),
            source,
            "a null zone_connect must be refused before the footprint writer runs"
        );

        let (result, _, _) = handler
            .dispatch_tool(
                "edit_footprint_pad",
                &json!({
                    "footprint_path": footprint.display().to_string(),
                    "pad_number": "1",
                    "match_all": true,
                    "width": 6.6,
                    "height": 6.6
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        let written = std::fs::read_to_string(&footprint).unwrap();
        assert_eq!(
            written.matches("(size 6.6 6.6)").count(),
            3,
            "the other edit applies"
        );
        assert_eq!(
            written.matches("(zone_connect 2)").count(),
            3,
            "omitting zone_connect keeps every pad's own override"
        );
    }

    #[tokio::test]
    async fn a_pad_typo_is_refused_without_creating_a_footprint() {
        let handler = handler().await;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("open-schema.kicad_mod");
        let (result, _, kind) = handler
            .dispatch_tool(
                "create_footprint",
                &json!({
                    "output": output.display().to_string(),
                    "name": "OpenSchema",
                    "pads": [{
                        "number": "1",
                        "type": "smd",
                        "shape": "rect",
                        "x": 0.0,
                        "y": 0.0,
                        "width": 1.0,
                        "height": 1.0,
                        "rotaton": 90.0
                    }]
                }),
            )
            .await;

        assert_eq!(kind.as_deref(), Some("invalid_argument"));
        assert!(result.is_error);
        assert_eq!(error_json(&result)["error"]["field"], "pads[0].rotaton");
        assert!(!output.exists(), "refusal must precede creation");
        let (result, _, _) = handler
            .dispatch_tool(
                "create_footprint",
                &json!({
                    "output": output.display().to_string(), "name": "Corrected",
                    "pads": [{"number": "1", "type": "smd", "shape": "rect", "x": 0, "y": 0,
                        "width": 1, "height": 1, "rotation": 90}]
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert!(std::fs::read_to_string(&output)
            .unwrap()
            .contains("(at 0 0 90)"));
    }

    #[tokio::test]
    async fn a_search_argument_typo_is_refused_over_json_rpc() {
        let handler = handler().await;
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 546, "method": "tools/call",
                "params": {"name": "search_symbols", "arguments": {"query": "R", "part_name": "R"}}
            }))
            .await
            .unwrap();
        let result: CallToolResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(result.is_error);
        let body = error_json(&result);
        assert_eq!(body["error"]["kind"], "invalid_argument");
        assert_eq!(body["error"]["field"], "part_name");
    }

    #[tokio::test]
    async fn meta_tool_typos_are_refused() {
        let handler = handler().await;
        assert_invalid_field(&handler, "get_recent_calls", json!({"limti": 5}), "limti").await;
    }

    #[tokio::test]
    async fn custom_fields_and_fixed_field_placements_have_distinct_contracts() {
        let handler = handler().await;
        let directory = tempfile::tempdir().unwrap();
        let schematic = directory.path().join("ecc83.kicad_sch");
        let source = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");
        std::fs::write(&schematic, source).unwrap();
        let args = json!({
            "schematic": schematic.display().to_string(), "reference": "U1",
            "fields": {"My private supplier": "Example Electronics"}
        });
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 547, "method": "tools/call",
                "params": {"name": "edit_schematic_component", "arguments": args}
            }))
            .await
            .unwrap();
        let result: CallToolResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(!result.is_error, "{result:?}");
        let after = std::fs::read_to_string(&schematic).unwrap();
        assert!(after.contains("(property \"My private supplier\" \"Example Electronics\""));
        assert_invalid_field(
            &handler,
            "edit_schematic_component",
            json!({
                "schematic": schematic.display().to_string(), "reference": "U1",
                "field_placements": {"My private supplier": {"rotaton": 90}}
            }),
            "field_placements.My private supplier.rotaton",
        )
        .await;
        assert_eq!(std::fs::read_to_string(&schematic).unwrap(), after);
    }

    #[tokio::test]
    async fn net_maps_and_arbitrary_configuration_values_remain_accepted() {
        let handler = handler().await;
        for (tool, args) in [
            (
                "apply_template",
                json!({"schematic": "missing.kicad_sch", "template_id": "ldo_3v3", "net_mappings": {"My net": "Other net"}}),
            ),
            (
                "copy_routing_pattern",
                json!({"board": "missing.kicad_pcb", "src_x1": 0, "src_y1": 0, "src_x2": 1, "src_y2": 1, "dest_x": 2, "dest_y": 2, "net_map": {"My net": "Other net"}}),
            ),
        ] {
            let (_, _, kind) = handler.dispatch_tool(tool, &args).await;
            assert_ne!(kind.as_deref(), Some("invalid_argument"), "{tool}");
        }
        // No write to real preferences: test the advertised validator directly.
        let tool = handler
            .ctx
            .router
            .get_tool("save_user_config")
            .await
            .unwrap();
        assert!(tool.input_validator.is_valid(&json!({"key_path": "private", "value": {"properties": {"type": "object", "free": [1, 2]}}})));
    }
}

#[cfg(test)]
mod first_missing_required_tests {
    use super::*;

    fn schema(required: Value) -> Value {
        json!({ "type": "object", "required": required })
    }

    #[test]
    fn names_them_in_schema_order_not_argument_order() {
        let s = schema(json!(["board", "net_name", "layer"]));
        assert_eq!(
            first_missing_required(&s, &json!({ "layer": "F.Cu" })).as_deref(),
            Some("board")
        );
        assert_eq!(
            first_missing_required(&s, &json!({ "board": "b.kicad_pcb" })).as_deref(),
            Some("net_name")
        );
    }

    /// An explicit `null` is a caller who has not supplied the argument —
    /// every `as_str()`/`as_array()` read would treat it that way, so the
    /// check must too, or the two paths disagree.
    #[test]
    fn an_explicit_null_counts_as_absent() {
        assert_eq!(
            first_missing_required(&schema(json!(["board"])), &json!({ "board": null })).as_deref(),
            Some("board")
        );
    }

    #[test]
    fn nothing_missing_when_all_are_present() {
        assert_eq!(
            first_missing_required(
                &schema(json!(["board", "net_name"])),
                &json!({ "board": "b", "net_name": "GND" })
            ),
            None
        );
    }

    /// A value of the wrong type is still *present*. This check is about
    /// presence; naming the field for a bad type is the handler's job.
    #[test]
    fn a_wrong_type_is_present_and_passes_through_to_the_handler() {
        assert_eq!(
            first_missing_required(&schema(json!(["query"])), &json!({ "query": 123 })),
            None
        );
    }

    #[test]
    fn a_schema_with_no_required_list_never_reports_one() {
        assert_eq!(
            first_missing_required(&json!({ "type": "object" }), &json!({})),
            None
        );
        assert_eq!(first_missing_required(&schema(json!([])), &json!({})), None);
    }
}

/// Claude Desktop caches the first `tools/list` and never re-fetches it, so
/// out of the box it could call 20 of 217 tools (#459). The handshake now
/// tells us who is asking, and a known caching client gets the whole
/// catalogue before its first listing.
#[cfg(test)]
mod client_adaptation_tests {
    use super::*;
    use crate::tools::ServerConfig;

    async fn lazy_handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        })
        .await
        .expect("handler builds")
    }

    fn request(method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: method.to_string(),
            params: Some(params),
        }
    }

    async fn listed_tool_count(handler: &McpHandler) -> usize {
        let out = handler
            .dispatch(&request("tools/list", json!({})))
            .await
            .expect("tools/list dispatches")
            .expect("tools/list returns a result");
        out["tools"].as_array().expect("tools array").len()
    }

    fn initialize_from(client: &str) -> Value {
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": client, "version": "0.1.0"}
        })
    }

    fn full_catalogue() -> usize {
        crate::router::registry::ALL_TOOLSETS
            .iter()
            .map(|t| t.tool_count)
            .sum::<usize>()
            + meta_tools::meta_tool_descriptions().len()
    }

    /// The exact string Claude Desktop sends, read from its own log.
    #[tokio::test]
    async fn claude_desktop_gets_the_full_catalogue_before_its_first_listing() {
        let handler = lazy_handler().await;
        let starter = listed_tool_count(&handler).await;
        handler
            .dispatch(&request("initialize", initialize_from("claude-ai")))
            .await
            .expect("initialize dispatches");
        let after = listed_tool_count(&handler).await;
        assert!(
            after > starter,
            "initialize must have loaded more than the starter kit: {starter} -> {after}"
        );
        assert_eq!(
            after,
            full_catalogue(),
            "a caching client must see the whole catalogue in its first listing"
        );
    }

    /// A client that honours list_changed keeps the cheap starter kit; the
    /// context economy is the point of the router and must survive this.
    #[tokio::test]
    async fn other_clients_keep_the_starter_kit() {
        let handler = lazy_handler().await;
        let starter = listed_tool_count(&handler).await;
        handler
            .dispatch(&request("initialize", initialize_from("some-other-client")))
            .await
            .expect("initialize dispatches");
        assert_eq!(listed_tool_count(&handler).await, starter);
    }

    /// No clientInfo at all is neither an error nor a reason to load.
    #[tokio::test]
    async fn a_missing_client_name_is_tolerated() {
        let handler = lazy_handler().await;
        let starter = listed_tool_count(&handler).await;
        handler
            .dispatch(&request("initialize", json!({})))
            .await
            .expect("initialize dispatches");
        assert_eq!(listed_tool_count(&handler).await, starter);
    }

    struct FixedProbe(Option<&'static str>);

    impl crate::guidance::GuidanceProbe for FixedProbe {
        fn detail(&self) -> Value {
            json!({"probe_status": "ok"})
        }

        fn take_notice(&self) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    async fn initialize_result(handler: &McpHandler) -> Value {
        handler
            .dispatch(&request("initialize", initialize_from("claude-code")))
            .await
            .expect("initialize dispatches")
            .expect("initialize returns a result")
    }

    /// Drift reaches the model through `instructions` (#728); with nothing
    /// to report, or no probe at all, the field is absent rather than empty.
    #[tokio::test]
    async fn initialize_carries_a_guidance_notice_only_when_there_is_one() {
        let bare = lazy_handler().await;
        assert!(initialize_result(&bare).await.get("instructions").is_none());

        let quiet = lazy_handler().await;
        quiet.set_guidance_probe(Arc::new(FixedProbe(None)));
        assert!(initialize_result(&quiet)
            .await
            .get("instructions")
            .is_none());

        let drifted = lazy_handler().await;
        drifted.set_guidance_probe(Arc::new(FixedProbe(Some("run konnect init"))));
        let result = initialize_result(&drifted).await;
        assert_eq!(result["instructions"], "run konnect init");
        assert_eq!(result["serverInfo"]["name"], "konnect");
    }

    /// Claude Code honours list_changed and must not be swept up by a loose
    /// "claude" match: the cost is ~23K tokens on every listing.
    #[test]
    fn matching_is_case_insensitive_prefix_and_does_not_catch_claude_code() {
        assert!(client_caches_tool_list("claude-ai"));
        assert!(client_caches_tool_list("Claude-AI"));
        assert!(client_caches_tool_list("claude-ai-desktop"));
        assert!(!client_caches_tool_list("claude-code"));
        assert!(!client_caches_tool_list("Claude Code"));
        assert!(!client_caches_tool_list(""));
    }
}

#[cfg(test)]
mod config_state_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    async fn call(handler: &McpHandler, tool: &str, arguments: Value) -> (bool, Value) {
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            }))
            .await
            .expect("tools/call receives a response");
        let result = response.result.expect("successful JSON-RPC response");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns JSON text");
        (
            result["isError"] == json!(true),
            serde_json::from_str(text).expect("tool body is JSON"),
        )
    }

    /// #580 through the served boundary: a project configuration that exists
    /// and cannot be parsed is a structured refusal from every tool that reads
    /// it, and no tool that writes it replaces it with the defaults.
    #[tokio::test]
    async fn a_malformed_project_configuration_is_refused_and_never_overwritten() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".konnect").join("project.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = r#"{"design_rules": ["keep me"],"#;
        std::fs::write(&config, original).unwrap();
        let project_dir = dir.path().display().to_string();

        for (tool, arguments) in [
            ("load_project_config", json!({ "project_dir": project_dir })),
            (
                "save_project_config",
                json!({ "project_dir": project_dir, "key_path": "fab.house", "value": "JLCPCB" }),
            ),
            (
                "add_design_rule",
                json!({ "project_dir": project_dir, "rule": "second", "scope": "project" }),
            ),
        ] {
            let (is_error, body) = call(&handler, tool, arguments).await;
            assert!(is_error, "{tool}: {body}");
            assert_eq!(
                body["error"]["kind"], "invalid_configuration",
                "{tool}: {body}"
            );
            assert!(
                body["error"]["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("malformed_json:")),
                "{tool}: {body}"
            );
            assert_eq!(
                std::fs::read_to_string(&config).unwrap(),
                original,
                "{tool} left the user's file exactly as it was"
            );
        }
    }

    #[tokio::test]
    async fn an_absent_project_configuration_says_defaults_and_a_save_creates_it() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().display().to_string();

        let (is_error, loaded) = call(
            &handler,
            "load_project_config",
            json!({ "project_dir": project_dir }),
        )
        .await;
        assert!(!is_error, "{loaded}");
        assert_eq!(loaded["source"], "defaults", "{loaded}");
        assert!(
            !dir.path().join(".konnect").exists(),
            "a load of an absent project file writes nothing"
        );

        let (is_error, saved) = call(
            &handler,
            "save_project_config",
            json!({ "project_dir": project_dir, "key_path": "fab.house", "value": "JLCPCB" }),
        )
        .await;
        assert!(!is_error, "{saved}");
        assert_eq!(saved["created"], true, "{saved}");

        let (_, reloaded) = call(
            &handler,
            "load_project_config",
            json!({ "project_dir": project_dir }),
        )
        .await;
        assert_eq!(reloaded["source"], "file", "{reloaded}");
        assert_eq!(reloaded["config"]["fab"]["house"], "JLCPCB", "{reloaded}");
    }
}

#[cfg(test)]
mod erc_report_shape_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    /// KiCad-authored fixture copied from the official `complex_hierarchy` demo.
    const KICAD_AUTHORED_SCHEMATIC: &str =
        include_str!("../../tests/fixtures/project_ownership/complex_hierarchy.kicad_sch");

    /// A stand-in kicad-cli that exits 0 and writes `report` to the `--output`
    /// path (`sch erc --output <path> …`, so the fourth argument).
    fn erc_cli_writing(dir: &std::path::Path, stem: &str, report: &str) -> std::path::PathBuf {
        crate::tools::cli::test_support::write_script(
            dir,
            stem,
            &format!("#!/bin/sh\nprintf '%s' '{report}' > \"$4\"\nexit 0\n"),
            &format!("@echo off\r\n> \"%~4\" echo {report}\r\nexit /b 0\r\n"),
        )
    }

    async fn run_erc_through_dispatch(stem: &str, report: &str) -> (bool, String) {
        let control = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let schematic = project.path().join("clock.kicad_sch");
        std::fs::write(&schematic, KICAD_AUTHORED_SCHEMATIC).unwrap();
        let cli = erc_cli_writing(control.path(), stem, report);

        let handler = McpHandler::new(ServerConfig {
            kicad_cli: cli.display().to_string(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "run_erc",
                    "arguments": { "schematic": schematic.display().to_string() }
                }
            }))
            .await
            .expect("tools/call receives a response");
        let result = response.result.expect("successful JSON-RPC response");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns text")
            .to_string();
        (result["isError"] == json!(true), text)
    }

    /// #581 through the served boundary: a kicad-cli that exits 0 and writes
    /// something that is not an ERC report must not come back as a clean ERC.
    #[tokio::test]
    async fn an_unrecognised_erc_report_is_an_error_not_zero_violations() {
        for (stem, report) in [
            ("erc-empty-object", "{}"),
            ("erc-drc-shaped", r#"{"violations":[]}"#),
            ("erc-no-sheets", r#"{"sheets":[]}"#),
            (
                "erc-sheet-without-violations",
                r#"{"sheets":[{"path":"/"}]}"#,
            ),
        ] {
            let (is_error, text) = run_erc_through_dispatch(stem, report).await;
            assert!(is_error, "{report} was served as a result: {text}");
            assert!(text.contains("ERC report"), "{report}: {text}");
            assert!(
                !text.contains("\"total\""),
                "{report}: no violation count may be reported: {text}"
            );
        }
    }

    /// The control: a report kicad-cli could write for a clean schematic is a
    /// clean result, so the refusal above is about shape and not about zero.
    #[tokio::test]
    async fn a_conforming_clean_report_is_still_a_clean_result() {
        let (is_error, text) =
            run_erc_through_dispatch("erc-clean", r#"{"sheets":[{"path":"/","violations":[]}]}"#)
                .await;
        assert!(!is_error, "{text}");
        let body: Value = serde_json::from_str(&text).expect("tool body is JSON");
        assert_eq!(body["total"], 0, "{body}");
        assert_eq!(body["errors"], 0, "{body}");
    }
}

#[cfg(test)]
mod sheet_pin_geometry_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    const ROOT: &str =
        include_str!("../../tests/fixtures/sheet_pin_edges/sheet_pin_edges.kicad_sch");
    const CHILD: &str = include_str!("../../tests/fixtures/sheet_pin_edges/child.kicad_sch");
    const LEFT_PIN: &str = "(at 101.6 106.68 180)";

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .unwrap()
    }

    async fn validate(handler: &McpHandler, placement: &str, child_present: bool) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sheet_pin_edges.kicad_sch");
        let child = dir.path().join("child.kicad_sch");
        assert_eq!(ROOT.matches(LEFT_PIN).count(), 1);
        let source = ROOT.replace(LEFT_PIN, placement);
        std::fs::write(&root, &source).unwrap();
        if child_present {
            std::fs::write(&child, CHILD).unwrap();
        }
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 687, "method": "tools/call",
                "params": { "name": "validate_sheet_pins", "arguments": { "schematic": root } }
            }))
            .await
            .expect("served tools/call response");
        let result = response.result.expect("JSON-RPC result");
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(
            std::fs::read_to_string(&root).unwrap(),
            source,
            "validator must not repair the parent"
        );
        if child_present {
            assert_eq!(
                std::fs::read_to_string(&child).unwrap(),
                CHILD,
                "validator must not change the child"
            );
        } else {
            assert!(
                !child.exists(),
                "validator must not create the missing child"
            );
        }
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn sheet_pin_geometry_reports_the_silent_net_merge_repro_through_dispatch() {
        let report = validate(&handler().await, "(at 101.6 106.68 0)", true).await;
        assert_eq!(report["issue_count"], 1, "{report}");
        let issue = &report["issues"][0];
        assert_eq!(issue["kind"], "pin_position_mismatch");
        assert_eq!(issue["sheet"], "child");
        assert_eq!(issue["pin"], "A");
        assert_eq!(issue["file"], "child.kicad_sch");
        assert!(issue["schematic"]
            .as_str()
            .unwrap()
            .ends_with("sheet_pin_edges.kicad_sch"));
        assert_eq!(
            issue["stored_position"],
            json!({"x":101.6,"y":106.68,"rotation":0.0})
        );
        let edge = &issue["expected_edge"];
        assert_eq!(edge["side"], "right");
        assert_eq!(edge["axis"], "x");
        assert_eq!(edge["span_axis"], "y");
        for (field, expected) in [
            ("coordinate", 152.4),
            ("span_start", 101.6),
            ("span_end", 132.08),
        ] {
            assert!((edge[field].as_f64().unwrap() - expected).abs() < 1e-6);
        }
        assert!(issue["recovery"]
            .as_str()
            .unwrap()
            .contains("edit_sheet_pin"));
    }

    #[tokio::test]
    async fn sheet_pin_geometry_checks_all_edges_and_their_spans_through_dispatch() {
        let handler = handler().await;
        for (placement, side) in [
            ("(at 101.6 106.68 0)", "right"),
            ("(at 152.4 106.68 180)", "left"),
            ("(at 110 132.08 90)", "top"),
            ("(at 110 101.6 270)", "bottom"),
            ("(at 101.6 99 180)", "left"),
            ("(at 152.4 134 0)", "right"),
            ("(at 99 101.6 90)", "top"),
            ("(at 154 132.08 270)", "bottom"),
        ] {
            let report = validate(&handler, placement, true).await;
            assert_eq!(report["issue_count"], 1, "{placement}: {report}");
            assert_eq!(report["issues"][0]["expected_edge"]["side"], side);
        }
        for placement in [
            LEFT_PIN,
            "(at 152.4 110 0)",
            "(at 110 101.6 90)",
            "(at 110 132.08 270)",
            "(at 101.6 101.6 180)",
            "(at 152.4 132.08 0)",
            "(at 101.6000005 106.68 180)",
        ] {
            let report = validate(&handler, placement, true).await;
            assert_eq!(report["issue_count"], 0, "{placement}: {report}");
        }
    }

    #[tokio::test]
    async fn sheet_pin_geometry_checks_parent_even_when_child_is_missing() {
        let report = validate(&handler().await, "(at 101.6 106.68 0)", false).await;
        assert_eq!(report["issue_count"], 2, "{report}");
        assert_eq!(report["issues"][0]["kind"], "pin_position_mismatch");
        assert_eq!(report["issues"][1]["error"], "child file not found on disk");
    }

    #[tokio::test]
    async fn sheet_pin_geometry_reports_a_rotation_that_names_no_edge() {
        let report = validate(&handler().await, "(at 101.6 106.68 45)", true).await;
        assert_eq!(report["issue_count"], 1, "{report}");
        assert_eq!(report["issues"][0]["kind"], "invalid_pin_rotation");
        assert_eq!(report["issues"][0]["stored_position"]["rotation"], 45.0);
        assert!(report["issues"][0]["expected_edge"].is_null());
    }
}

#[cfg(test)]
mod ipc_uncertainty_dispatch_tests {
    use super::*;
    use crate::test_support::MockIpcServer;
    use crate::tools::{pcb_board::board_mock::board_document, ServerConfig};
    use konnect_ipc::{builders::pack_any, gen::kiapi};

    #[tokio::test]
    async fn served_write_preserves_unknown_outcome_and_saved_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        let before = include_str!("../../../konnect-sexp/tests/fixtures/gr_poly_outline.kicad_pcb");
        std::fs::write(&board, before).unwrap();
        let document = board_document(&board.to_string_lossy());
        let mock = MockIpcServer::spawn("unknown-write", move |request| {
            let command = request.message.unwrap();
            if command.type_url.ends_with("GetOpenDocuments") {
                kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: Some(pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![document.clone()],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )),
                }
            } else {
                kiapi::common::ApiResponse {
                    status: None,
                    header: None,
                    message: None,
                }
            }
        });
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: mock.address().to_string(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .unwrap();
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 658, "method": "tools/call",
                "params": { "name": "add_mounting_hole", "arguments": {
                    "board": board.to_string_lossy(), "x": 5.0, "y": 6.0, "reference": "H1"
                }}
            }))
            .await
            .unwrap();
        let result = response.result.unwrap();
        assert_eq!(result["isError"], true, "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["error"]["kind"], "ipc_outcome_unknown", "{value}");
        assert_eq!(value["error"]["retry_safe"], false);
        assert_eq!(value["error"]["board_state"], "unknown");
        assert!(!text.contains("was not modified"));
        assert_eq!(std::fs::read_to_string(board).unwrap(), before);
    }
}

#[cfg(test)]
mod component_properties_dispatch_tests {
    use super::*;
    use crate::tools::ServerConfig;

    const SHEET: &str = include_str!("../../tests/fixtures/component_fields_kicad10.kicad_sch");
    const R1_UUID: &str = "8b9b7750-3421-4b18-92a3-4bddb63e9056";
    const U1_UNIT_1: &str = "db4e5e3c-9adc-40eb-a748-d1a1a0578a45";

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    async fn call(handler: &McpHandler, tool: &str, arguments: Value) -> Value {
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 679,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            }))
            .await
            .expect("tools/call receives a response");
        let result = response.result.expect("successful JSON-RPC response");
        assert_ne!(result["isError"], json!(true), "{result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool returns JSON text");
        serde_json::from_str(text).expect("tool body is JSON")
    }

    fn schematic(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("properties.kicad_sch");
        std::fs::write(&path, SHEET).unwrap();
        path.display().to_string()
    }

    /// R1 as the fixture stores it: every existing field of its list row, and
    /// its properties with the standard and empty ones included.
    fn r1_row() -> Value {
        json!({
            "reference": "R1",
            "value": "10k",
            "footprint": "",
            "lib_id": "Device:R",
            "x": 101.6,
            "y": 76.2,
            "rotation": 0.0,
            "mirror_x": false,
            "mirror_y": false,
            "properties": {
                "Reference": "R1",
                "Value": "10k",
                "Footprint": "",
                "Datasheet": "",
                "Description": "Resistor",
                "LCSC": "C679001",
                "MPN": "MPN-679-R1"
            }
        })
    }

    /// `properties` is a public response field (#679); prove KiCad-saved
    /// `LCSC` and `MPN` survive the served `tools/call` boundary and leave
    /// every existing field as it was.
    #[tokio::test]
    async fn get_reports_custom_properties_across_the_served_dispatch() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let schematic = schematic(&dir);

        let r1 = call(
            &handler,
            "get_schematic_component",
            json!({ "schematic": schematic, "reference": "R1" }),
        )
        .await;
        let mut expected = r1_row();
        expected["uuid"] = json!(R1_UUID);
        expected["unit_count"] = json!(1);
        expected["units"] = json!([{
            "unit": 1,
            "x": 101.6,
            "y": 76.2,
            "rotation": 0.0,
            "mirror_x": false,
            "mirror_y": false,
            "uuid": R1_UUID
        }]);
        assert_eq!(r1, expected);

        // KiCad saved U1's unit 2 first; the component is still unit 1.
        let u1 = call(
            &handler,
            "get_schematic_component",
            json!({ "schematic": schematic, "reference": "U1" }),
        )
        .await;
        assert_eq!(u1["uuid"], U1_UNIT_1, "{u1}");
        assert_eq!(u1["properties"]["LCSC"], "C679002", "{u1}");
        assert_eq!(u1["properties"]["MPN"], "MPN-679-U1", "{u1}");
    }

    /// The same through `list_schematic_components`, one object on each placed
    /// unit's row.
    #[tokio::test]
    async fn list_reports_custom_properties_across_the_served_dispatch() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let listed = call(
            &handler,
            "list_schematic_components",
            json!({ "schematic": schematic(&dir) }),
        )
        .await;
        assert_eq!(listed["count"], 4, "{listed}");
        for row in listed["components"].as_array().unwrap() {
            match row["reference"].as_str() {
                Some("R1") => assert_eq!(*row, r1_row()),
                Some("U1") => {
                    assert_eq!(row["properties"]["LCSC"], "C679002", "{listed}");
                    assert_eq!(row["properties"]["MPN"], "MPN-679-U1", "{listed}");
                }
                other => panic!("unexpected reference {other:?}: {listed}"),
            }
        }
    }
}

#[cfg(test)]
mod label_children_dispatch_tests {
    //! An unrelated schematic edit must keep every child KiCad wrote on a label,
    //! `(fields_autoplaced …)` in particular, in the form the file used (#695).
    use super::*;
    use crate::tools::ServerConfig;

    const KICAD7: &str =
        include_str!("../../../konnect-schematic-editor/tests/fixtures/labels_kicad7.kicad_sch");
    const KICAD9: &str =
        include_str!("../../../konnect-schematic-editor/tests/fixtures/labels_kicad9.kicad_sch");

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .unwrap()
    }

    /// Serve one `add_wire` far from every label and return the file it wrote.
    async fn add_unrelated_wire(source: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let sheet = dir.path().join("labels.kicad_sch");
        std::fs::write(&sheet, source).unwrap();
        let response = handler()
            .await
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 695, "method": "tools/call",
                "params": { "name": "add_wire", "arguments": {
                    "schematic": sheet, "x1": 12.7, "y1": 12.7, "x2": 25.4, "y2": 12.7
                } }
            }))
            .await
            .expect("served tools/call response");
        let result = response.result.expect("JSON-RPC result");
        assert_ne!(result["isError"], true, "{result}");
        let written = std::fs::read_to_string(&sheet).unwrap();
        assert!(written.contains("(wire"), "the edit was not written");
        written
    }

    /// The text that follows `anchor` in `text`, from its next child on.
    fn after<'a>(text: &'a str, anchor: &str) -> &'a str {
        let at = text
            .find(anchor)
            .unwrap_or_else(|| panic!("{anchor} missing"));
        text[at + anchor.len()..].trim_start()
    }

    #[tokio::test]
    async fn a_kicad8_or_later_token_keeps_its_value_and_position() {
        let written = add_unrelated_wire(KICAD9).await;
        assert_eq!(written.matches("(fields_autoplaced yes)").count(), 1);
        assert!(after(&written, "(at 226.695 151.13 180)").starts_with("(fields_autoplaced yes)"));
        // The label that never had the token still does not.
        let cc2 = &written[written.find("(label \"CC2\"").unwrap()..];
        assert!(!cc2[..cc2.find("(uuid").unwrap()].contains("fields_autoplaced"));
    }

    #[tokio::test]
    async fn a_kicad7_bare_token_stays_bare_on_both_label_types() {
        let written = add_unrelated_wire(KICAD7).await;
        assert_eq!(
            written.matches("(fields_autoplaced)").count(),
            2,
            "{written}"
        );
        assert!(!written.contains("fields_autoplaced yes"), "{written}");
        assert!(after(&written, "(at 187.96 275.59 0)").starts_with("(fields_autoplaced)"));
        assert!(after(&written, "(at 394.97 69.85 0)").starts_with("(fields_autoplaced)"));
    }
}

#[cfg(test)]
mod zone_deletion_dispatch_tests {
    use super::*;
    use crate::test_support::MockIpcServer;
    use crate::tools::pcb_board::board_mock::board_document;
    use crate::tools::ServerConfig;
    use konnect_ipc::{builders::pack_any, gen::kiapi};
    use prost::Message;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const ZONE_FIXTURE: &[u8] =
        include_bytes!("../../../konnect-sexp/tests/fixtures/zone_outline_elements.kicad_pcb");
    const ZONE_IDS: [&str; 3] = [
        "9a05f3f6-3b48-4653-80ad-b87c8f0311e2",
        "5735a8ec-4c09-4701-8363-571f8dfc4353",
        "3fbf99d0-27e9-4110-a352-2139dde74d30",
    ];

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    async fn call(handler: &McpHandler, id: u64, name: &str, arguments: Value) -> CallToolResult {
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }))
            .await
            .expect("request returns a response");
        assert!(response.error.is_none(), "tool errors are MCP results");
        serde_json::from_value(response.result.expect("tools/call returns a result"))
            .expect("result uses the advertised MCP shape")
    }

    fn body(result: &CallToolResult) -> Value {
        let text = match result.content.first() {
            Some(ToolContent::Text { text }) => text,
            other => panic!("expected JSON text, got {other:?}"),
        };
        serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
    }

    const LIVE_BOARD_FIXTURE: &[u8] =
        include_bytes!("../../../konnect-sexp/tests/fixtures/ecc83-pp.kicad_pcb");
    const LIVE_ZONE_PAYLOAD: &[u8] =
        include_bytes!("../../tests/fixtures/issue_474_copper_zone_0.ipc.bin");
    const LIVE_ZONE_UUID: &str = "00000000-0000-0000-0000-00004eed97a2";
    const RULE_AREA_PAYLOAD: &[u8] =
        include_bytes!("../../tests/fixtures/issue_474_zone_0.ipc.bin");
    const RULE_AREA_BOARD_FIXTURE: &[u8] =
        include_bytes!("../../../konnect-ipc/tests/fixtures/live_ipc.kicad_pcb");

    #[derive(Clone)]
    struct MockBoard {
        path: String,
        original: String,
        after_delete: String,
        zones: Vec<prost_types::Any>,
    }

    struct LiveMockState {
        boards: Vec<MockBoard>,
        deleted: std::collections::HashSet<String>,
        deleted_zone_ids: HashMap<String, std::collections::HashSet<String>>,
        delete_ids: Vec<String>,
        requests: Vec<String>,
        get_items_reads: HashMap<String, usize>,
        wrong_type_after_list: bool,
        fail_postdelete_get_items: bool,
        postdelete_busy_reads: usize,
        reject_refill: bool,
        empty_items: bool,
        change_snapshot_during_next_inventory: bool,
        snapshot_change_save_reads: HashMap<String, usize>,
        no_open_documents: bool,
        change_revision_on_begin_commit: bool,
        revision_changed: bool,
        fail_rollback: bool,
        end_commit_actions: Vec<i32>,
        refill_targets: Vec<String>,
    }

    fn board_path(document: &kiapi::common::types::DocumentSpecifier) -> String {
        match document.identifier.as_ref() {
            Some(kiapi::common::types::document_specifier::Identifier::BoardFilename(path)) => {
                path.clone()
            }
            other => panic!("expected a board document identifier, got {other:?}"),
        }
    }

    fn live_fixture_board(path: &std::path::Path) -> MockBoard {
        live_fixture_board_with(
            path,
            LIVE_BOARD_FIXTURE,
            vec![live_zone_payload()],
            LIVE_ZONE_UUID,
        )
    }

    fn live_zone_payload() -> prost_types::Any {
        prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Zone".to_string(),
            value: LIVE_ZONE_PAYLOAD.to_vec(),
        }
    }

    fn rule_area_payload() -> prost_types::Any {
        prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Zone".to_string(),
            value: RULE_AREA_PAYLOAD.to_vec(),
        }
    }

    fn captured_zone_uuid(payload: &[u8]) -> String {
        kiapi::board::types::Zone::decode(payload)
            .expect("captured zone payload decodes")
            .id
            .expect("captured zone has a UUID")
            .value
    }

    fn native_fixture_with_copper_and_rule_area() -> String {
        let rule_uuid = captured_zone_uuid(RULE_AREA_PAYLOAD);
        let rule_board = String::from_utf8_lossy(RULE_AREA_BOARD_FIXTURE);
        let marker = format!("(uuid \"{rule_uuid}\")");
        let uuid_offset = rule_board
            .find(&marker)
            .expect("captured rule-area UUID exists in its native fixture");
        let zone_start = rule_board[..uuid_offset]
            .rfind("(zone")
            .expect("captured UUID belongs to a native zone");
        let (start, end) =
            konnect_sexp::writer::find_block_with_leading_whitespace(&rule_board, zone_start)
                .expect("captured rule area has a complete native source span");
        let rule_block = &rule_board[start..end];

        let mut combined = String::from_utf8_lossy(LIVE_BOARD_FIXTURE).into_owned();
        let root_close = combined
            .rfind(')')
            .expect("native copper fixture has a root closing delimiter");
        combined.insert_str(root_close, &format!("\n{rule_block}\n"));
        combined
    }

    fn live_fixture_board_with(
        path: &std::path::Path,
        fixture: &[u8],
        zones: Vec<prost_types::Any>,
        delete_uuid: &str,
    ) -> MockBoard {
        let original = String::from_utf8_lossy(fixture).into_owned();
        let uuid_marker = format!("(uuid \"{delete_uuid}\")");
        let uuid_offset = original
            .find(&uuid_marker)
            .expect("target UUID exists in the native fixture");
        let start = original[..uuid_offset]
            .rfind("(zone")
            .expect("target UUID belongs to a native zone");
        let (start, end) =
            konnect_sexp::writer::find_block_with_leading_whitespace(&original, start)
                .expect("native fixture zone has a complete source span");
        let after_delete = konnect_sexp::writer::apply_edits(
            original.clone(),
            vec![konnect_sexp::writer::SexpEdit::delete(start, end)],
        );
        MockBoard {
            path: path.to_string_lossy().into_owned(),
            original,
            after_delete,
            zones,
        }
    }

    fn live_mock(boards: Vec<MockBoard>) -> (MockIpcServer, Arc<Mutex<LiveMockState>>) {
        let state = Arc::new(Mutex::new(LiveMockState {
            boards: boards.clone(),
            deleted: Default::default(),
            deleted_zone_ids: HashMap::new(),
            delete_ids: Vec::new(),
            requests: Vec::new(),
            get_items_reads: HashMap::new(),
            wrong_type_after_list: false,
            fail_postdelete_get_items: false,
            postdelete_busy_reads: 0,
            reject_refill: false,
            empty_items: false,
            change_snapshot_during_next_inventory: false,
            snapshot_change_save_reads: HashMap::new(),
            no_open_documents: false,
            change_revision_on_begin_commit: false,
            revision_changed: false,
            fail_rollback: false,
            end_commit_actions: Vec::new(),
            refill_targets: Vec::new(),
        }));
        let shared = state.clone();
        let documents = boards
            .iter()
            .map(|board| board_document(&board.path))
            .collect::<Vec<_>>();
        let server = MockIpcServer::spawn("served-zone-delete", move |request| {
            let command = request.message.expect("IPC request has a command");
            let command_name = command.type_url.rsplit('.').next().unwrap_or_default();
            let mut state = shared.lock().unwrap();
            let ok = |message| kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsOk as i32,
                    error_message: String::new(),
                }),
                header: None,
                message,
            };
            let refused =
                |status: kiapi::common::ApiStatusCode, error: &str| kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: status as i32,
                        error_message: error.to_string(),
                    }),
                    header: None,
                    message: None,
                };

            match command_name {
                "GetOpenDocuments" => ok(Some(pack_any(
                    &kiapi::common::commands::GetOpenDocumentsResponse {
                        documents: if state.no_open_documents {
                            Vec::new()
                        } else {
                            documents.clone()
                        },
                    },
                    "kiapi.common.commands.GetOpenDocumentsResponse",
                ))),
                "SaveDocumentToString" => {
                    let query = kiapi::common::commands::SaveDocumentToString::decode(
                        command.value.as_slice(),
                    )
                    .expect("SaveDocumentToString request");
                    let document = query.document.expect("document target");
                    let path = board_path(&document);
                    state.requests.push(format!("save:{path}"));
                    let board = state
                        .boards
                        .iter()
                        .find(|board| board.path == path)
                        .expect("target board is among open documents");
                    let mut contents = if state.deleted.contains(&path) {
                        board.after_delete.clone()
                    } else {
                        board.original.clone()
                    };
                    if state.change_snapshot_during_next_inventory {
                        let count = state
                            .snapshot_change_save_reads
                            .entry(path.clone())
                            .or_default();
                        *count += 1;
                        if *count == 2 {
                            contents.push('\n');
                            state.change_snapshot_during_next_inventory = false;
                        }
                    }
                    if state.revision_changed && !state.deleted.contains(&path) {
                        contents.push('\n');
                    }
                    ok(Some(pack_any(
                        &kiapi::common::commands::SavedDocumentResponse {
                            document: Some(document),
                            contents,
                        },
                        "kiapi.common.commands.SavedDocumentResponse",
                    )))
                }
                "GetItems" => {
                    let query = kiapi::common::commands::GetItems::decode(command.value.as_slice())
                        .expect("GetItems request");
                    let document = query
                        .header
                        .and_then(|header| header.document)
                        .expect("GetItems document target");
                    let path = board_path(&document);
                    state.requests.push(format!("items:{path}"));
                    let count = {
                        let count = state.get_items_reads.entry(path.clone()).or_default();
                        *count += 1;
                        *count
                    };
                    if state.deleted.contains(&path) && state.postdelete_busy_reads > 0 {
                        state.postdelete_busy_reads -= 1;
                        return refused(
                            kiapi::common::ApiStatusCode::AsBusy,
                            "native refill still running",
                        );
                    }
                    if state.deleted.contains(&path) && state.fail_postdelete_get_items {
                        return refused(
                            kiapi::common::ApiStatusCode::AsBadRequest,
                            "post-delete item read failed",
                        );
                    }
                    let board = state
                        .boards
                        .iter()
                        .find(|board| board.path == path)
                        .expect("target board is among open documents");
                    let deleted_ids = state.deleted_zone_ids.get(&path);
                    let mut items = board
                        .zones
                        .iter()
                        .filter(|item| {
                            let decoded = kiapi::board::types::Zone::decode(item.value.as_slice())
                                .expect("mock zone payload decodes");
                            let id = decoded.id.map(|id| id.value).unwrap_or_default();
                            !deleted_ids.is_some_and(|ids| ids.contains(&id))
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if state.empty_items && !state.deleted.contains(&path) {
                        items.clear();
                    }
                    if state.wrong_type_after_list && count > 1 {
                        for item in &mut items {
                            item.type_url =
                                "type.googleapis.com/kiapi.board.types.Track".to_string();
                        }
                    }
                    ok(Some(pack_any(
                        &kiapi::common::commands::GetItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            items,
                        },
                        "kiapi.common.commands.GetItemsResponse",
                    )))
                }
                "BeginCommit" => {
                    state.requests.push("begin_commit".into());
                    if state.change_revision_on_begin_commit {
                        state.revision_changed = true;
                    }
                    ok(Some(pack_any(
                        &kiapi::common::commands::BeginCommitResponse {
                            id: Some(kiapi::common::types::Kiid {
                                value: "zone-test-commit".into(),
                            }),
                        },
                        "kiapi.common.commands.BeginCommitResponse",
                    )))
                }
                "DeleteItems" => {
                    let delete =
                        kiapi::common::commands::DeleteItems::decode(command.value.as_slice())
                            .expect("DeleteItems request");
                    let path = board_path(
                        &delete
                            .header
                            .expect("DeleteItems header")
                            .document
                            .expect("document"),
                    );
                    state.requests.push(format!("delete:{path}"));
                    state
                        .delete_ids
                        .extend(delete.item_ids.iter().map(|id| id.value.clone()));
                    state
                        .deleted_zone_ids
                        .entry(path.clone())
                        .or_default()
                        .extend(delete.item_ids.iter().map(|id| id.value.clone()));
                    state.deleted.insert(path);
                    ok(Some(pack_any(
                        &kiapi::common::commands::DeleteItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            deleted_items: Vec::new(),
                        },
                        "kiapi.common.commands.DeleteItemsResponse",
                    )))
                }
                "EndCommit" => {
                    let end = kiapi::common::commands::EndCommit::decode(command.value.as_slice())
                        .expect("EndCommit request");
                    state.end_commit_actions.push(end.action);
                    if state.fail_rollback
                        && end.action == kiapi::common::commands::CommitAction::CmaDrop as i32
                    {
                        return refused(
                            kiapi::common::ApiStatusCode::AsBadRequest,
                            "rollback rejected",
                        );
                    }
                    ok(Some(pack_any(
                        &kiapi::common::commands::EndCommitResponse {},
                        "kiapi.common.commands.EndCommitResponse",
                    )))
                }
                "RefillZones" if state.reject_refill => refused(
                    kiapi::common::ApiStatusCode::AsBadRequest,
                    "zone refill rejected",
                ),
                "RefillZones" => {
                    let refill =
                        kiapi::board::commands::RefillZones::decode(command.value.as_slice())
                            .expect("RefillZones request");
                    let document = refill.board.expect("RefillZones board target");
                    state.refill_targets.push(board_path(&document));
                    ok(None)
                }
                other => panic!("unexpected IPC command in zone test: {other}"),
            }
        });
        (server, state)
    }

    async fn handler_at(ipc_address: String) -> McpHandler {
        McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address,
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds")
    }

    #[tokio::test]
    async fn served_zone_saved_delete_removes_exactly_one_observed_zone() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("zones.kicad_pcb");
        std::fs::write(&board, ZONE_FIXTURE).unwrap();
        let path = board.display().to_string();
        let before = std::fs::read(&board).unwrap();

        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "saved"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        assert_eq!(observed["source"], "saved_board");
        let zones = observed["zones"].as_array().unwrap();
        assert_eq!(zones.len(), 3);
        let observed_ids = zones
            .iter()
            .map(|zone| zone["uuid"].as_str().unwrap())
            .collect::<Vec<_>>();
        for expected in ZONE_IDS {
            assert!(
                observed_ids.contains(&expected),
                "fixture zone {expected} must be listed"
            );
        }
        let target = zones[0]["uuid"].as_str().unwrap();
        let revision = observed["revision"].as_str().unwrap();

        let deleted = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": path,
                "uuid": target,
                "revision": revision,
                "source": "saved_board"
            }),
        )
        .await;
        assert!(!deleted.is_error, "{}", body(&deleted));
        assert_eq!(body(&deleted)["outcome"]["status"], "complete");

        let after = call(
            &handler,
            3,
            "list_zones",
            json!({"board": path, "board_source": "saved"}),
        )
        .await;
        assert!(!after.is_error, "{}", body(&after));
        let remaining = body(&after)["zones"].as_array().unwrap().clone();
        assert_eq!(remaining.len(), 2);
        assert!(!remaining.iter().any(|zone| zone["uuid"] == target));
        for zone in &zones[1..] {
            assert!(remaining.contains(zone), "unrelated zone changed: {zone}");
        }

        let written = std::fs::read(&board).unwrap();
        let prefix = before
            .iter()
            .zip(&written)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = before[prefix..]
            .iter()
            .rev()
            .zip(written[prefix..].iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        let removed = String::from_utf8_lossy(&before[prefix..before.len() - suffix]);
        let inserted = &written[prefix..written.len() - suffix];
        assert!(
            inserted.is_empty(),
            "zone deletion inserted or rewrote bytes"
        );
        assert_eq!(removed.matches("(zone").count(), 1);
        assert!(removed.contains(&format!("(uuid \"{target}\")")));
        for zone in &remaining {
            assert!(String::from_utf8_lossy(&written).contains(zone["uuid"].as_str().unwrap()));
        }
    }

    #[tokio::test]
    async fn served_zone_saved_observation_refuses_when_same_board_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("zones.kicad_pcb");
        std::fs::write(&board, ZONE_FIXTURE).unwrap();
        let mock_board = live_fixture_board(&board);
        let (server, state) = live_mock(vec![mock_board]);
        let handler = handler_at(server.address().to_string()).await;
        let path = board.to_string_lossy().to_string();

        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "saved"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        assert_eq!(observed["source"], "saved_board");
        let before_requests = state.lock().unwrap().requests.len();
        let before = std::fs::read(&board).unwrap();

        let result = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": path,
                "uuid": observed["zones"][0]["uuid"],
                "revision": observed["revision"],
                "source": "saved_board"
            }),
        )
        .await;
        assert!(result.is_error, "saved observation must not switch to live");
        assert_eq!(std::fs::read(&board).unwrap(), before);
        let state = state.lock().unwrap();
        let delete_requests = &state.requests[before_requests..];
        assert!(
            !delete_requests
                .iter()
                .any(|request| request.starts_with("items:")),
            "source-switch refusal must precede live zone inventory: {delete_requests:?}"
        );
        assert!(!delete_requests
            .iter()
            .any(|request| request == "begin_commit"));
        assert!(!delete_requests
            .iter()
            .any(|request| request.starts_with("delete:")));
        assert!(state.delete_ids.is_empty());
    }

    #[tokio::test]
    async fn served_zone_stale_revision_and_unknown_uuid_refuse_without_writing() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("zones.kicad_pcb");
        std::fs::write(&board, ZONE_FIXTURE).unwrap();
        let path = board.display().to_string();
        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "saved"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        let uuid = observed["zones"][0]["uuid"].clone();
        let revision = observed["revision"].as_str().unwrap();
        for (id, candidate_uuid, candidate_revision) in [
            ("stale", uuid, json!(format!("{revision}-stale"))),
            (
                "unknown",
                json!("00000000-0000-0000-0000-000000000000"),
                observed["revision"].clone(),
            ),
        ] {
            let before = std::fs::read(&board).unwrap();
            let result = call(
                &handler,
                if id == "stale" { 2 } else { 3 },
                "delete_zone",
                json!({
                    "board": path,
                    "uuid": candidate_uuid,
                    "revision": candidate_revision,
                    "source": "saved_board"
                }),
            )
            .await;
            assert!(result.is_error, "{id} observation must be refused");
            assert_eq!(
                std::fs::read(&board).unwrap(),
                before,
                "{id} refusal wrote the board"
            );
        }
    }

    #[tokio::test]
    async fn served_zone_delete_refuses_track_uuid_without_writing() {
        const BOARD_FIXTURE: &[u8] =
            include_bytes!("../../../konnect-ipc/tests/fixtures/live_ipc.kicad_pcb");
        const TRACK_UUID: &str = "b1f4c0de-0000-4000-8000-00000000ad01";

        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("live-ipc-fixture.kicad_pcb");
        std::fs::write(&board, BOARD_FIXTURE).unwrap();
        let before = std::fs::read(&board).unwrap();
        let fixture = String::from_utf8_lossy(&before).replace("\r\n", "\n");
        let uuid_marker = format!("(uuid \"{TRACK_UUID}\")");
        let uuid_offset = fixture
            .find(&uuid_marker)
            .expect("hardcoded UUID exists in fixture");
        let segment_start = fixture[..uuid_offset]
            .rfind("(segment")
            .expect("UUID belongs to a preceding segment record");
        let segment_end = fixture[uuid_offset..]
            .find("\n\t)")
            .map(|offset| uuid_offset + offset)
            .expect("segment record has a closing delimiter");
        let segment = &fixture[segment_start..segment_end];
        assert!(segment.contains(&uuid_marker));
        assert!(segment.contains("(start 145 105)"));
        assert!(segment.contains("(end 195 105)"));
        let path = board.display().to_string();
        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "saved"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        assert_eq!(observed["source"], "saved_board");
        assert_eq!(observed["zones"].as_array().unwrap().len(), 2);
        assert!(!observed["zones"]
            .as_array()
            .unwrap()
            .iter()
            .any(|zone| zone["uuid"] == TRACK_UUID));

        let result = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": path,
                "uuid": TRACK_UUID,
                "revision": observed["revision"],
                "source": "saved_board"
            }),
        )
        .await;
        assert!(
            result.is_error,
            "a track UUID must not be accepted as a zone"
        );
        assert_eq!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn served_zone_schema_rejects_invalid_delete_arguments_before_file_write() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("zones.kicad_pcb");
        std::fs::write(&board, ZONE_FIXTURE).unwrap();
        let before = std::fs::read(&board).unwrap();
        let result = call(
            &handler,
            1,
            "delete_zone",
            json!({
                "board": board.display().to_string(),
                "uuid": "9a05f3f6-3b48-4653-80ad-b87c8f0311e2",
                "revision": 4,
                "source": "saved_board"
            }),
        )
        .await;
        assert!(result.is_error);
        let error = body(&result);
        assert_eq!(error["error"]["field"], "revision");
        assert_eq!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn served_zone_live_inventory_uses_native_snapshot_and_stale_revision_never_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("live.kicad_pcb");
        std::fs::write(&board, LIVE_BOARD_FIXTURE).unwrap();
        let mock_board = live_fixture_board(&board);
        let (server, state) = live_mock(vec![mock_board]);
        let handler = handler_at(server.address().to_string()).await;
        let path = board.to_string_lossy().to_string();

        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "live"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        assert_eq!(observed["source"], "ipc");
        assert_eq!(observed["zones"].as_array().unwrap().len(), 1);
        assert_eq!(observed["zones"][0]["uuid"], LIVE_ZONE_UUID);
        assert_eq!(observed["zones"][0]["kind"], "copper");

        let stale = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": path,
                "uuid": LIVE_ZONE_UUID,
                "revision": "stale-observation",
                "source": "ipc"
            }),
        )
        .await;
        assert!(stale.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&stale).as_deref(),
            Some("stale_target")
        );
        let state = state.lock().unwrap();
        assert!(
            state.delete_ids.is_empty(),
            "stale revision reached DeleteItems"
        );
        assert!(!state
            .requests
            .iter()
            .any(|request| request.starts_with("delete:")));
    }

    #[tokio::test]
    async fn served_zone_live_observation_refuses_if_board_closes_before_delete() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("live.kicad_pcb");
        std::fs::write(&board, LIVE_BOARD_FIXTURE).unwrap();
        let (server, state) = live_mock(vec![live_fixture_board(&board)]);
        let handler = handler_at(server.address().to_string()).await;
        let path = board.to_string_lossy().to_string();
        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "live"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        let before_requests = state.lock().unwrap().requests.len();
        state.lock().unwrap().no_open_documents = true;

        let result = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": path,
                "uuid": LIVE_ZONE_UUID,
                "revision": observed["revision"],
                "source": "ipc"
            }),
        )
        .await;
        assert!(result.is_error, "closed-board observation must be refused");
        let state = state.lock().unwrap();
        let delete_requests = &state.requests[before_requests..];
        assert!(!delete_requests
            .iter()
            .any(|request| request.starts_with("items:")));
        assert!(!delete_requests
            .iter()
            .any(|request| request == "begin_commit"));
        assert!(!delete_requests
            .iter()
            .any(|request| request.starts_with("delete:")));
        assert!(state.delete_ids.is_empty());
    }

    #[tokio::test]
    async fn served_zone_live_inconsistent_inventory_and_snapshot_race_refuse_before_commit() {
        for fault in ["empty_items", "changing_snapshot"] {
            let dir = tempfile::tempdir().unwrap();
            let board = dir.path().join("live.kicad_pcb");
            std::fs::write(&board, LIVE_BOARD_FIXTURE).unwrap();
            let (server, state) = live_mock(vec![live_fixture_board(&board)]);
            let handler = handler_at(server.address().to_string()).await;
            let path = board.to_string_lossy().to_string();
            let listing = call(
                &handler,
                1,
                "list_zones",
                json!({"board": path, "board_source": "live"}),
            )
            .await;
            assert!(!listing.is_error, "{}", body(&listing));
            let observed = body(&listing);
            {
                let mut state = state.lock().unwrap();
                state.empty_items = fault == "empty_items";
                state.change_snapshot_during_next_inventory = fault == "changing_snapshot";
            }

            let result = call(
                &handler,
                2,
                "delete_zone",
                json!({
                    "board": path,
                    "uuid": LIVE_ZONE_UUID,
                    "revision": observed["revision"],
                    "source": "ipc"
                }),
            )
            .await;
            assert!(result.is_error, "{fault} must refuse deletion");
            let state = state.lock().unwrap();
            assert!(
                !state
                    .requests
                    .iter()
                    .any(|request| request == "begin_commit"),
                "{fault} passed the complete inventory/snapshot guard"
            );
            assert!(!state
                .requests
                .iter()
                .any(|request| request.starts_with("delete:")));
            assert!(state.delete_ids.is_empty());
        }
    }

    #[tokio::test]
    async fn served_zone_live_revision_change_inside_commit_skips_delete_and_failed_rollback_is_uncertain(
    ) {
        for fail_rollback in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let board = dir.path().join("live.kicad_pcb");
            std::fs::write(&board, LIVE_BOARD_FIXTURE).unwrap();
            let (server, state) = live_mock(vec![live_fixture_board(&board)]);
            let handler = handler_at(server.address().to_string()).await;
            let path = board.to_string_lossy().to_string();
            let listing = call(
                &handler,
                1,
                "list_zones",
                json!({"board": path, "board_source": "live"}),
            )
            .await;
            assert!(!listing.is_error, "{}", body(&listing));
            let observed = body(&listing);
            {
                let mut state = state.lock().unwrap();
                state.change_revision_on_begin_commit = true;
                state.fail_rollback = fail_rollback;
            }

            let result = call(
                &handler,
                2,
                "delete_zone",
                json!({
                    "board": path,
                    "uuid": LIVE_ZONE_UUID,
                    "revision": observed["revision"],
                    "source": "ipc"
                }),
            )
            .await;
            assert!(result.is_error, "changed in-commit revision must fail");
            let outcome = body(&result);
            assert_eq!(
                outcome["outcome"]["status"],
                if fail_rollback { "uncertain" } else { "failed" }
            );
            let state = state.lock().unwrap();
            assert!(state
                .requests
                .iter()
                .any(|request| request == "begin_commit"));
            assert_eq!(
                state.end_commit_actions,
                vec![kiapi::common::commands::CommitAction::CmaDrop as i32],
                "failed prewrite validation attempts rollback"
            );
            assert!(!state
                .requests
                .iter()
                .any(|request| request.starts_with("delete:")));
            assert!(state.delete_ids.is_empty());
        }
    }

    #[tokio::test]
    async fn served_zone_live_wrong_any_type_refuses_without_delete_or_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("live.kicad_pcb");
        std::fs::write(&board, LIVE_BOARD_FIXTURE).unwrap();
        let before = std::fs::read(&board).unwrap();
        let mock_board = live_fixture_board(&board);
        let (server, state) = live_mock(vec![mock_board]);
        let handler = handler_at(server.address().to_string()).await;
        let path = board.to_string_lossy().to_string();
        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "live"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        state.lock().unwrap().wrong_type_after_list = true;

        let result = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": path,
                "uuid": LIVE_ZONE_UUID,
                "revision": observed["revision"],
                "source": "ipc"
            }),
        )
        .await;
        assert!(result.is_error, "wrong Any.type_url must refuse deletion");
        assert!(state.lock().unwrap().delete_ids.is_empty());
        assert_eq!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn served_zone_live_binds_rule_area_refill_and_remaining_copper_to_exact_board() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.kicad_pcb");
        let second = dir.path().join("second.kicad_pcb");
        std::fs::write(&first, LIVE_BOARD_FIXTURE).unwrap();
        let combined_fixture = native_fixture_with_copper_and_rule_area();
        std::fs::write(&second, combined_fixture.as_bytes()).unwrap();
        let rule_uuid = captured_zone_uuid(RULE_AREA_PAYLOAD);
        let (server, state) = live_mock(vec![
            live_fixture_board(&first),
            live_fixture_board_with(
                &second,
                combined_fixture.as_bytes(),
                vec![live_zone_payload(), rule_area_payload()],
                &rule_uuid,
            ),
        ]);
        let handler = handler_at(server.address().to_string()).await;
        let first_path = first.to_string_lossy().to_string();
        let second_path = second.to_string_lossy().to_string();

        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": second_path, "board_source": "live"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        assert_eq!(observed["source"], "ipc");
        assert_eq!(observed["zones"].as_array().unwrap().len(), 2);
        let rule_area = observed["zones"]
            .as_array()
            .unwrap()
            .iter()
            .find(|zone| zone["uuid"] == rule_uuid)
            .expect("captured rule area is inventoried");
        assert_eq!(rule_area["kind"], "rule_area");
        let before_first = std::fs::read(&first).unwrap();
        let before_second = std::fs::read(&second).unwrap();
        let deleted = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": second_path,
                "uuid": rule_uuid,
                "revision": observed["revision"],
                "source": "ipc"
            }),
        )
        .await;
        assert!(!deleted.is_error, "{}", body(&deleted));
        assert_eq!(body(&deleted)["outcome"]["status"], "complete");
        assert_eq!(body(&deleted)["deleted"], true);
        assert_eq!(body(&deleted)["unrelated_zones_preserved"], true);
        assert_eq!(body(&deleted)["remaining_copper_filled"], true);
        assert_eq!(std::fs::read(&first).unwrap(), before_first);
        assert_eq!(std::fs::read(&second).unwrap(), before_second);
        let state = state.lock().unwrap();
        let requests = &state.requests;
        assert!(requests
            .iter()
            .any(|request| request == &format!("items:{second_path}")));
        assert!(requests
            .iter()
            .any(|request| request == &format!("save:{second_path}")));
        assert!(requests
            .iter()
            .any(|request| request == &format!("delete:{second_path}")));
        assert!(!requests
            .iter()
            .any(|request| request.ends_with(&first_path)));
        assert!(!state.deleted.contains(&first_path));
        assert!(state.deleted.contains(&second_path));
        assert_eq!(state.delete_ids, vec![rule_uuid]);
        assert_eq!(state.refill_targets, vec![second_path]);
    }

    async fn served_live_delete_result(reject_refill: bool, fail_postdelete_read: bool) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("live.kicad_pcb");
        std::fs::write(&board, LIVE_BOARD_FIXTURE).unwrap();
        let mock_board = live_fixture_board(&board);
        let (server, state) = live_mock(vec![mock_board]);
        {
            let mut state = state.lock().unwrap();
            state.reject_refill = reject_refill;
            state.postdelete_busy_reads = if reject_refill { 0 } else { 2 };
            state.fail_postdelete_get_items = fail_postdelete_read;
        }
        let handler = handler_at(server.address().to_string()).await;
        let path = board.to_string_lossy().to_string();
        let listing = call(
            &handler,
            1,
            "list_zones",
            json!({"board": path, "board_source": "live"}),
        )
        .await;
        assert!(!listing.is_error, "{}", body(&listing));
        let observed = body(&listing);
        let result = call(
            &handler,
            2,
            "delete_zone",
            json!({
                "board": path,
                "uuid": LIVE_ZONE_UUID,
                "revision": observed["revision"],
                "source": "ipc"
            }),
        )
        .await;
        assert!(!result.is_error, "{}", body(&result));
        let outcome = body(&result);
        assert_eq!(
            state.lock().unwrap().delete_ids,
            vec![LIVE_ZONE_UUID.to_string()]
        );
        outcome
    }

    #[tokio::test]
    async fn served_zone_live_failed_refill_with_verified_delete_reports_partial() {
        let outcome = served_live_delete_result(true, false).await;
        assert_eq!(outcome["outcome"]["status"], "partial");
        assert_eq!(outcome["deleted"], true);
        assert_eq!(outcome["readback_verified"], true);
        assert_eq!(outcome["refill_request_accepted"], false);
    }

    #[tokio::test]
    async fn served_zone_live_waits_for_busy_item_reads_before_reporting_refill_complete() {
        let outcome = served_live_delete_result(false, false).await;
        assert_eq!(outcome["outcome"]["status"], "complete");
        assert_eq!(outcome["deleted"], true);
        assert_eq!(outcome["readback_verified"], true);
        assert_eq!(outcome["refill_request_accepted"], true);
        assert_eq!(outcome["refill_completed"], true);
    }

    #[tokio::test]
    async fn served_zone_live_failed_readback_after_refill_rejection_reports_uncertain() {
        let outcome = served_live_delete_result(true, true).await;
        assert_eq!(outcome["outcome"]["status"], "uncertain");
        assert!(outcome["deleted"].is_null());
        assert_eq!(outcome["readback_verified"], false);
        assert_eq!(outcome["refill_request_accepted"], false);
    }
}
