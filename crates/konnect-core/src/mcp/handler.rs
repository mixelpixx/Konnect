//! McpHandler — receives raw JSON messages from any transport and dispatches
//! to the correct MCP method handler or tool executor.

use super::error::{extract_error_kind, ToolErrorKind};
use super::protocol::*;
use super::server::McpServerState;
use crate::observability::{
    default_calls_log_path, new_call_id, unix_ms, CallObserver, CallRecord, CallStatus,
};
use crate::router::{meta_tools, ToolRouter};
use axum::response::sse::Event;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

/// Clone-able handle to the MCP request handler.
/// Multiple transports (STDIO + HTTP) share the same handler.
#[derive(Clone)]
pub struct McpHandler {
    ctx: Arc<crate::tools::ToolContext>,
    sse_senders: Arc<RwLock<Vec<mpsc::Sender<Event>>>>,
    /// Raw-JSON-line notification sinks for non-SSE transports (stdio). A
    /// server-initiated notification (e.g. tools/list_changed) must reach the
    /// active transport; SSE senders only cover HTTP, so stdio registers here
    /// to receive the same notifications. Without this, notifications are
    /// silently dropped on stdio — the cause of issue #19.
    notif_sinks: Arc<RwLock<Vec<mpsc::Sender<String>>>>,
    observer: CallObserver,
}

impl McpHandler {
    pub async fn new(config: crate::tools::ServerConfig) -> anyhow::Result<Self> {
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
        let ctx = Arc::new(crate::tools::ToolContext::new_with_observer(
            config,
            router,
            observer.clone(),
        ));

        Ok(McpHandler {
            ctx,
            sse_senders: Arc::new(RwLock::new(Vec::new())),
            notif_sinks: Arc::new(RwLock::new(Vec::new())),
            observer,
        })
    }

    /// Accessor for the `CallObserver` — used by meta-tools `get_recent_calls`
    /// and `server_stats` that live on `ToolContext`.
    pub fn observer(&self) -> &CallObserver {
        &self.observer
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

    async fn dispatch(&self, req: &JsonRpcRequest) -> anyhow::Result<Option<Value>> {
        match req.method.as_str() {
            // ── Lifecycle ──────────────────────────────────────────────────
            "initialize" => {
                let result = McpServerState::build_initialize_result();
                Ok(Some(serde_json::to_value(result)?))
            }
            "notifications/initialized" => Ok(None),
            "ping" => Ok(Some(json!({}))),

            // ── Tool listing ───────────────────────────────────────────────
            "tools/list" => {
                // Meta-tools (always visible) + all domain tools (pre-loaded at startup)
                let mut tools = meta_tools::meta_tool_descriptions();
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
        if let Some(result) = meta_tools::handle_meta_tool(name, args, &self.ctx).await {
            if name == "load_toolset" || name == "unload_toolset" {
                self.notify_tools_list_changed().await;
            }
            let status = if result.is_error {
                CallStatus::Error
            } else {
                CallStatus::Ok
            };
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
            return match (tool_def.handler)(args, self.ctx.clone()).await {
                Ok(result) => {
                    let status = if result.is_error {
                        CallStatus::Error
                    } else {
                        CallStatus::Ok
                    };
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
                    let path = kicad_editor_locked_path(&e)
                        .expect("guard matched")
                        .display()
                        .to_string();
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
