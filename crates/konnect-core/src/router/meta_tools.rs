//! The 7 always-visible meta-tools.
//!
//! Discovery / routing:
//!   list_toolboxes()          — show all 18 toolsets with descriptions and load state
//!   load_toolset(name)        — activate a toolset, expose its tools in tools/list
//!   unload_toolset(name)      — deactivate a toolset, remove its tools from tools/list
//!   get_active_toolsets()     — list currently loaded toolsets
//!
//! Maintenance:
//!   reload_server(confirm)    — exec into the binary on disk, keeping the connection
//!
//! Observability:
//!   get_recent_calls(limit?)  — last N tool calls (newest first) with timing + status
//!   server_stats()            — uptime, per-tool totals/errors, JSONL log path
//!
//! At server startup only the STARTER_KIT (`project`, `config`) is pre-loaded so
//! baseline context stays small. The LLM reads `list_toolboxes` and calls
//! `load_toolset(name)` to expose the tools it actually needs for the task.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::{CallToolResult, McpToolDescription};
use crate::tools::ToolContext;
use serde_json::{json, Value};

/// Return the 7 meta-tool MCP descriptions (always in the tools/list response).
pub fn meta_tool_descriptions() -> Vec<McpToolDescription> {
    vec![
        McpToolDescription {
            name: "list_toolboxes".to_string(),
            description:
                "List all available KiCAD toolsets with descriptions, categories, tool counts, \
                 and whether each is currently loaded. Only the starter kit (project, config) \
                 is loaded at startup — call load_toolset(name) to expose additional tools \
                 in subsequent tools/list responses. Always call this first to discover what \
                 tools are available for the task."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpToolDescription {
            name: "load_toolset".to_string(),
            description:
                "Load a toolset by name so its tools appear in tools/list and can be called. \
                 Returns the list of tools that were added. Use list_toolboxes() first to \
                 see valid names. Pass an array to load several toolsets in one call -- \
                 cheaper, one tools/list refresh."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "anyOf": [
                            {"type": "string"},
                            {"type": "array", "items": {"type": "string"}}
                        ],
                        "description": "Toolset name (e.g. 'sch_components', 'pcb_routing'), or an array of names"
                    }
                },
                "required": ["name"]
            }),
        },
        McpToolDescription {
            name: "unload_toolset".to_string(),
            description: "Unload a toolset to remove its tools from the active session. \
                 Use this to keep the tool list manageable when switching tasks. \
                 With auto_load_toolsets enabled, tools reload on use."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Toolset name to unload"
                    }
                },
                "required": ["name"]
            }),
        },
        McpToolDescription {
            name: "get_active_toolsets".to_string(),
            description:
                "Return the list of currently loaded toolsets and how many tools each provides."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpToolDescription {
            name: "get_recent_calls".to_string(),
            description:
                "Return the most recent tool calls this session (newest first) with call_id, \
                 tool name, toolset, duration, status (ok/error/not_found), and \
                 error_kind when failed. Use this to self-diagnose — e.g. 'why did the last call \
                 fail?' or 'what tools have I been running?'"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Max number of calls to return (default 20, max 100). Pass 0 for all buffered calls.",
                        "default": 20
                    }
                },
                "required": []
            }),
        },
        McpToolDescription {
            name: "server_stats".to_string(),
            description:
                "Return server uptime, total/error call counts, per-tool statistics, and the \
                 path to the JSONL call log. Good for 'what's my error rate today?' and \
                 'which tool has been slowest?'."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpToolDescription {
            name: "reload_server".to_string(),
            description: "Restart the server in place from the binary on disk, so a freshly built \
                 Konnect takes effect without restarting the MCP client. The process image \
                 is replaced (same PID, same stdio pipes), so the connection survives. The \
                 new binary is verified before the switch, and the call is refused if it \
                 does not run. Loaded toolsets reset to the starter kit afterwards — reload \
                 the ones you need. Unix only."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "confirm": {
                        "type": "boolean",
                        "description": "Must be true. Guards against an accidental reload mid-task."
                    }
                },
                "required": ["confirm"]
            }),
        },
    ]
}

/// Attempt to handle a meta-tool call. Returns `None` if the name is not a meta-tool.
pub async fn handle_meta_tool(
    name: &str,
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> Option<CallToolResult> {
    match name {
        "list_toolboxes" => Some(handle_list_toolboxes(ctx).await),
        "load_toolset" => Some(handle_load_toolset(args, ctx).await),
        "unload_toolset" => Some(handle_unload_toolset(args, ctx).await),
        "get_active_toolsets" => Some(handle_get_active_toolsets(ctx).await),
        "get_recent_calls" => Some(handle_get_recent_calls(args, ctx).await),
        "server_stats" => Some(handle_server_stats(ctx).await),
        "reload_server" => Some(handle_reload_server(args).await),
        _ => None,
    }
}

/// Replace this process with a fresh copy of the binary on disk.
///
/// A stdio MCP server cannot meaningfully "restart itself": the client owns the
/// process, and exiting just drops the transport — the client does not respawn
/// it mid-session. `exec` sidesteps that. It replaces the process *image* while
/// keeping the PID and, crucially, the inherited stdin/stdout pipes, so the
/// client's connection is never broken and it goes on talking to what is now
/// the new build.
///
/// Two things the caller should know:
///
/// * Router state does not survive. The new image starts at the starter kit, so
///   previously loaded toolsets must be loaded again. A call to a tool that was
///   loaded before returns the usual `toolset_not_loaded` error naming its
///   toolset, so recovery is one hop.
/// * The reply is written before the switch. `exec` never returns on success,
///   so the response has to reach the client first; a short delay covers the
///   transport's flush.
async fn handle_reload_server(args: &Value) -> CallToolResult {
    if !args
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "confirm".to_string(),
                reason: "must be true — reload_server restarts the server in place".to_string(),
            },
            "reload_server requires confirm=true.",
        );
    }

    #[cfg(not(unix))]
    {
        CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::HandlerError {
                reason: "exec-in-place is Unix only".to_string(),
            },
            "reload_server is not supported on this platform: replacing the process image \
             while keeping the stdio pipes requires exec(), which Windows has no equivalent \
             for. Restart the MCP client instead.",
        )
    }

    #[cfg(unix)]
    {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                return CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::HandlerError {
                        reason: format!("cannot determine current executable: {e}"),
                    },
                    format!("reload_server could not find its own binary: {e}"),
                );
            }
        };

        // Verify before switching. `exec` is a one-way door: if the binary on
        // disk is broken — a half-written copy, a build that fails to link, an
        // unsigned binary macOS will kill — the server is simply gone and the
        // client has nothing to talk to. Running it once first turns that into
        // a refused call.
        match std::process::Command::new(&exe).arg("--version").output() {
            Ok(out) if out.status.success() => {
                let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
                tracing::info!(exe = %exe.display(), %version, "reload_server: exec into new image");

                let exe_for_task = exe.clone();
                tokio::spawn(async move {
                    // Let the transport flush this call's reply before the
                    // process image is replaced.
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    use std::os::unix::process::CommandExt;
                    let err = std::process::Command::new(&exe_for_task).exec();
                    // exec only returns on failure.
                    tracing::error!(error = %err, "reload_server: exec failed; server still running old image");
                });

                CallToolResult::json(&json!({
                    "reloading": true,
                    "binary": exe.display().to_string(),
                    "version": version,
                    "note": "Server is replacing its process image. The connection survives \
                             (same PID, same pipes). Loaded toolsets reset to the starter kit — \
                             call load_toolset again for the ones you need.",
                }))
            }
            Ok(out) => CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::HandlerError {
                    reason: format!("binary exited with {}", out.status),
                },
                format!(
                    "Refusing to reload: {} does not run cleanly (exit {}). \
                     stderr: {}",
                    exe.display(),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ),
            Err(e) => CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::HandlerError {
                    reason: format!("cannot execute binary: {e}"),
                },
                format!(
                    "Refusing to reload: {} could not be executed ({e}). On macOS a freshly \
                     copied binary needs re-signing: codesign --force -s - <path>",
                    exe.display()
                ),
            ),
        }
    }
}

async fn handle_list_toolboxes(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    use std::collections::HashSet;
    let active: HashSet<String> = ctx.router.active_names().await.into_iter().collect();

    let toolsets: Vec<Value> = ctx
        .router
        .all_toolsets()
        .iter()
        .map(|t| {
            let loaded = active.contains(t.name);
            json!({
                "name": t.name,
                "description": t.description,
                "category": t.category,
                "tool_count": t.tool_count,
                "loaded": loaded,
            })
        })
        .collect();

    CallToolResult::json(&json!({
        "toolsets": toolsets,
        "total_tools": toolsets.iter()
            .filter_map(|t| t["tool_count"].as_u64())
            .sum::<u64>(),
        "loaded_count": active.len(),
        "hint": "Only loaded toolsets contribute tools to tools/list. Call load_toolset(name) \
                 to expose a toolset's tools. Call unload_toolset(name) to prune tools you no \
                 longer need (keeps context small).",
    }))
}

async fn handle_load_toolset(args: &Value, ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    match &args["name"] {
        // Legacy single-name form: result shape is byte-identical to the
        // pre-batch behavior (`loaded` is a string, `tools` echoes descriptions).
        Value::String(name) => match ctx.router.load(name).await {
            Some(tools) => {
                let tool_list: Vec<Value> = tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description }))
                    .collect();
                CallToolResult::json(&json!({
                    "loaded": name,
                    "tools_added": tools.len(),
                    "tools": tool_list
                }))
            }
            None => CallToolResult::error(format!(
                "Unknown toolset '{}'. Call list_toolboxes() to see valid names.",
                name
            )),
        },
        // New array form: one load, one tools/list_changed notification.
        Value::Array(arr) => {
            let mut names: Vec<String> =
                match arr.iter().map(|v| v.as_str().map(str::to_string)).collect() {
                    Some(names) => names,
                    None => return CallToolResult::error("name array must contain only strings"),
                };
            // Duplicate names in one call would double-count tools_added.
            let mut seen = std::collections::HashSet::new();
            names.retain(|n| seen.insert(n.clone()));

            let mut loaded = Vec::new();
            let mut tools_added = 0usize;
            let mut tool_list: Vec<Value> = Vec::new();
            let mut errors = Vec::new();

            for name in &names {
                match ctx.router.load(name).await {
                    Some(tools) => {
                        loaded.push(name.clone());
                        tools_added += tools.len();
                        tool_list.extend(
                            tools
                                .iter()
                                .map(|t| json!({ "name": t.name, "description": t.description })),
                        );
                    }
                    None => errors.push(format!(
                        "Unknown toolset '{}'. Call list_toolboxes() to see valid names.",
                        name
                    )),
                }
            }

            // Nothing loaded at all -- a typed error so the observer keeps a kind,
            // rather than a JSON body with a manually-set is_error flag.
            if loaded.is_empty() {
                let kind = ToolErrorKind::InvalidArgument {
                    field: "name".to_string(),
                    reason: names.join(", "),
                };
                return CallToolResult::error_kind(
                    kind,
                    format!(
                        "No toolsets loaded -- all names were unknown: {}. Call list_toolboxes() to see valid names.",
                        names.join(", ")
                    ),
                );
            }

            // Partial success (some names unknown, some loaded) is not an error --
            // the caller gets what loaded plus an errors array for the rest.
            CallToolResult::json(&json!({
                "loaded": loaded,
                "tools_added": tools_added,
                "tools": tool_list,
                "errors": errors,
            }))
        }
        _ => CallToolResult::error("Missing required argument: name (string or array of strings)"),
    }
}

async fn handle_unload_toolset(args: &Value, ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return CallToolResult::error("Missing required argument: name"),
    };

    if ctx.router.unload(name).await {
        CallToolResult::text(format!("Toolset '{}' unloaded.", name))
    } else {
        CallToolResult::error(format!("Unknown toolset '{}'.", name))
    }
}

async fn handle_get_recent_calls(
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> CallToolResult {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(20);
    let records = ctx.observer.recent(limit).await;
    let count = records.len();
    CallToolResult::json(&json!({
        "count": count,
        "limit_applied": if limit == 0 { count } else { limit },
        "calls": records,
        "hint": "Calls are ordered newest-first. Use server_stats for aggregates.",
    }))
}

async fn handle_server_stats(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let snap = ctx.observer.snapshot().await;
    CallToolResult::json(&snap)
}

async fn handle_get_active_toolsets(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let active = ctx.router.active_names().await;
    let all = ctx.router.all_toolsets();

    let result: Vec<Value> = active
        .iter()
        .filter_map(|name| {
            all.iter().find(|t| t.name == name.as_str()).map(|meta| {
                json!({
                    "name": meta.name,
                    "description": meta.description,
                    "tool_count": meta.tool_count
                })
            })
        })
        .collect();

    CallToolResult::json(&json!({
        "active_toolsets": result,
        "total_active_tools": result.iter()
            .filter_map(|t| t["tool_count"].as_u64())
            .sum::<u64>()
    }))
}

#[cfg(test)]
mod meta_tool_tests {
    use super::*;
    use crate::mcp::protocol::ToolContent;

    /// The meta-tool count is quoted in DEV.md, README.md and tool-directory.md
    /// ("187 registered + 6 meta = 193"). Pin it so adding one forces those to
    /// be updated in the same commit rather than drifting.
    #[test]
    fn meta_tool_count_is_pinned() {
        let names: Vec<String> = meta_tool_descriptions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(
            names.len(),
            7,
            "meta-tool count changed — update DEV.md, README.md and tool-directory.md too. \
             Current: {names:?}"
        );
    }

    /// Every meta-tool advertised in tools/list must actually dispatch, or the
    /// model sees a tool it cannot call.
    #[tokio::test]
    async fn every_advertised_meta_tool_dispatches() {
        use crate::router::ToolRouter;
        use crate::tools::{ServerConfig, ToolContext};
        use std::sync::Arc;

        let ctx = Arc::new(ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        ));

        for desc in meta_tool_descriptions() {
            // reload_server would replace the process; check dispatch only, with
            // confirm omitted so it returns the InvalidArgument guard instead.
            let args = json!({});
            let handled = handle_meta_tool(&desc.name, &args, &ctx).await;
            assert!(
                handled.is_some(),
                "meta-tool '{}' is advertised but not dispatched",
                desc.name
            );
        }
    }

    /// The guard exists so a stray call cannot restart the server mid-task.
    #[tokio::test]
    async fn reload_server_refuses_without_confirm() {
        let result = handle_reload_server(&json!({})).await;
        assert!(result.is_error);

        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        assert!(
            text.contains("confirm"),
            "error should name the missing confirm flag, got: {text}"
        );
    }
}
