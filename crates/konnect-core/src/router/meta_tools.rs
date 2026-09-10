//! The 7 cross-platform meta-tools, plus an optional Unix stdio reload tool.
//!
//! Discovery / routing:
//!   list_toolboxes()          — show every toolset with descriptions and load state
//!   load_toolset(name)        — activate a toolset, expose its tools in tools/list
//!   unload_toolset(name)      — deactivate a toolset, remove its tools from tools/list
//!   get_active_toolsets()     — list currently loaded toolsets
//!
//! Maintenance (Unix, stdio-only):
//!   reload_server(confirm)    — re-exec after the transport flushes the reply
//!
//! Observability:
//!   get_recent_calls(limit?)  — last N tool calls (newest first) with timing + status
//!   server_stats()            — uptime, per-tool totals/errors, JSONL log path
//!   get_installation_info()   — serving build, binary, install, KiCad, and IPC provenance
//!
//! At server startup only the STARTER_KIT (`project`, `config`) is pre-loaded so
//! baseline context stays small. The LLM reads `list_toolboxes` and calls
//! `load_toolset(name)` to expose the tools it actually needs for the task.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::{CallToolResult, McpToolDescription};
use crate::tools::ToolContext;
use serde_json::{json, Value};
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::fd::AsRawFd;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct ReloadPlan {
    /// User-facing path of the installed binary. The open handle below is the
    /// executable identity used for both validation and the eventual exec.
    pub binary_path: PathBuf,
    #[cfg(unix)]
    executable: File,
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    identity: ExecutableIdentity,
    pub arguments: Vec<OsString>,
}

impl ReloadPlan {
    /// Resolve the platform's safest handoff for the candidate that passed the
    /// version probe.
    #[cfg(unix)]
    pub fn validated_exec_path(&self) -> Result<PathBuf, String> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // Linux can execute the open file itself, so a concurrent rename
            // of the installation path cannot change the selected image.
            Ok(executable_handle_path(&self.executable))
        }

        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            // macOS and the remaining Unix targets expose no portable fexecve.
            // Recheck the open file and its pathname at the final handoff so a
            // normal atomic build/install replacement is refused.
            self.identity.verify(&self.executable, &self.binary_path)?;
            Ok(self.binary_path.clone())
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn executable_handle_path(executable: &File) -> PathBuf {
    PathBuf::from("/proc/self/fd").join(executable.as_raw_fd().to_string())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutableIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
impl ExecutableIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }

    fn capture(executable: &File, path: &std::path::Path) -> Result<Self, String> {
        let open = Self::from_metadata(
            &executable
                .metadata()
                .map_err(|error| format!("cannot inspect open candidate: {error}"))?,
        );
        let named = Self::from_metadata(
            &std::fs::metadata(path)
                .map_err(|error| format!("cannot inspect candidate path: {error}"))?,
        );
        if open != named {
            return Err("candidate path changed during validation".to_string());
        }
        Ok(open)
    }

    fn verify(&self, executable: &File, path: &std::path::Path) -> Result<(), String> {
        let current = Self::capture(executable, path)?;
        if current != *self {
            return Err("candidate executable changed after its version probe".to_string());
        }
        Ok(())
    }
}

#[cfg(any(unix, test))]
#[derive(Debug, PartialEq, Eq)]
enum ReloadVersionRefusal {
    Downgrade,
    SameVersionNeedsOptIn,
    Uncomparable,
}

#[cfg(any(unix, test))]
fn validate_reload_version(
    candidate: &str,
    running: &str,
    allow_same_version: bool,
) -> Result<(), ReloadVersionRefusal> {
    match crate::runtime_info::compare_konnect_versions(candidate, running) {
        Some(std::cmp::Ordering::Less) => Err(ReloadVersionRefusal::Downgrade),
        Some(std::cmp::Ordering::Equal) if !allow_same_version => {
            Err(ReloadVersionRefusal::SameVersionNeedsOptIn)
        }
        None => Err(ReloadVersionRefusal::Uncomparable),
        _ => Ok(()),
    }
}

#[cfg(any(unix, test))]
fn reload_arguments<I>(arguments: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    arguments.into_iter().skip(1).collect()
}

/// One-shot handoff between the core handler and the stdio transport. The
/// handler validates and queues a reload; the transport performs it only after
/// the JSON-RPC response and notifications have been flushed.
#[derive(Debug, Clone, Default)]
pub struct ReloadControl {
    enabled: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<ReloadPlan>>>,
}

impl ReloadControl {
    pub fn enable(&self) {
        self.enabled.store(true, AtomicOrdering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(AtomicOrdering::Acquire)
    }

    #[cfg(any(unix, test))]
    fn request(&self, plan: ReloadPlan) -> Result<(), &'static str> {
        let mut pending = self.pending.lock().map_err(|_| "reload state poisoned")?;
        if pending.is_some() {
            return Err("a reload is already pending");
        }
        *pending = Some(plan);
        Ok(())
    }

    pub fn take(&self) -> Option<ReloadPlan> {
        self.pending.lock().ok()?.take()
    }
}

/// Return the always-visible meta-tool descriptions. `reload_server` is added
/// only by the standalone Unix server when it runs stdio exclusively.
pub fn meta_tool_descriptions() -> Vec<McpToolDescription> {
    meta_tool_descriptions_for(false)
}

pub fn meta_tool_descriptions_for(reload_enabled: bool) -> Vec<McpToolDescription> {
    let descriptions = vec![
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
            name: "get_installation_info".to_string(),
            description:
                "Report read-only provenance for the Konnect process serving this call: build \
                 version and commit when available, executable path, conservatively detected \
                 install source, on-disk binary version, KiCad CLI version, redacted IPC \
                 endpoint, proven newer-binary state, and platform-specific restart guidance."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    ];

    #[cfg(unix)]
    {
        let mut descriptions = descriptions;
        if reload_enabled {
            descriptions.push(McpToolDescription {
            name: "reload_server".to_string(),
            description: "Replace this standalone Unix stdio server with the verified Konnect binary now on disk while preserving the client-owned pipes and original command-line arguments. The transport stops accepting requests and flushes this response, process bookkeeping is released, and then the validated executable identity is rechecked at the one-way exec handoff. Linux executes the open file directly. Loaded toolsets reset to startup state. Same-version development builds require allow_same_version=true."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "confirm": {
                        "type": "boolean",
                        "description": "Must be true. Guards against an accidental reload mid-task."
                    },
                    "allow_same_version": {
                        "type": "boolean",
                        "default": false,
                        "description": "Explicitly permit a same-version development rebuild. Downgrades are always refused."
                    }
                },
                "required": ["confirm"]
            }),
            });
        }
        descriptions
    }

    #[cfg(not(unix))]
    {
        let _ = reload_enabled;
        descriptions
    }
}

/// Attempt to handle a meta-tool call. Returns `None` if the name is not a meta-tool.
pub async fn handle_meta_tool(
    name: &str,
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> Option<CallToolResult> {
    handle_meta_tool_with_reload(name, args, ctx, &ReloadControl::default()).await
}

pub async fn handle_meta_tool_with_reload(
    name: &str,
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
    reload: &ReloadControl,
) -> Option<CallToolResult> {
    #[cfg(not(unix))]
    let _ = reload;
    match name {
        "list_toolboxes" => Some(handle_list_toolboxes(ctx).await),
        "load_toolset" => Some(handle_load_toolset(args, ctx).await),
        "unload_toolset" => Some(handle_unload_toolset(args, ctx).await),
        "get_active_toolsets" => Some(handle_get_active_toolsets(ctx).await),
        "get_recent_calls" => Some(handle_get_recent_calls(args, ctx).await),
        "server_stats" => Some(handle_server_stats(ctx).await),
        "get_installation_info" => Some(handle_get_installation_info(ctx).await),
        #[cfg(unix)]
        "reload_server" if reload.is_enabled() => Some(handle_reload_server(args, reload).await),
        _ => None,
    }
}

#[cfg(unix)]
async fn handle_reload_server(args: &Value, reload: &ReloadControl) -> CallToolResult {
    if args.get("confirm").and_then(Value::as_bool) != Some(true) {
        return CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "confirm".to_string(),
                reason: "must be true".to_string(),
            },
            "reload_server requires confirm=true.",
        );
    }

    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: format!("cannot determine current executable: {error}"),
                },
                format!("reload_server could not find its own binary: {error}"),
            )
        }
    };

    let candidate = match File::open(&executable) {
        Ok(file) => file,
        Err(error) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: format!("cannot open candidate executable: {error}"),
                },
                format!(
                    "reload_server could not open the Konnect binary at {}: {error}",
                    executable.display()
                ),
            )
        }
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let candidate_probe_path = executable_handle_path(&candidate);
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let candidate_probe_path = executable.clone();
    let disk_version = match crate::runtime_info::probe_konnect_version(&candidate_probe_path).await
    {
        Ok(version) => version,
        Err(status) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: format!("candidate probe failed: {status}"),
                },
                format!(
                "reload_server refused the binary at {} because its version probe was {status}.",
                executable.display()
            ),
            )
        }
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let identity = match ExecutableIdentity::capture(&candidate, &executable) {
        Ok(identity) => identity,
        Err(reason) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: reason.clone(),
                },
                format!("reload_server refused the candidate: {reason}"),
            )
        }
    };
    let running_version = env!("CARGO_PKG_VERSION");
    let allow_same_version = args.get("allow_same_version").and_then(Value::as_bool) == Some(true);
    match validate_reload_version(&disk_version, running_version, allow_same_version) {
        Err(ReloadVersionRefusal::Downgrade) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: format!(
                        "candidate version {disk_version} is older than running version {running_version}"
                    ),
                },
                "reload_server refuses to downgrade the serving process.",
            )
        }
        Err(ReloadVersionRefusal::SameVersionNeedsOptIn) => {
            return CallToolResult::error_kind(
                ToolErrorKind::InvalidArgument {
                    field: "allow_same_version".to_string(),
                    reason: format!(
                        "the candidate and running process both report {running_version}"
                    ),
                },
                "Set allow_same_version=true only when intentionally loading a development rebuild.",
            )
        }
        Err(ReloadVersionRefusal::Uncomparable) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: format!(
                        "cannot compare candidate version {disk_version} with running version {running_version}"
                    ),
                },
                "reload_server could not prove that the candidate is not a downgrade.",
            )
        }
        Ok(()) => {}
    }

    let plan = ReloadPlan {
        binary_path: executable.clone(),
        executable: candidate,
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        identity,
        arguments: reload_arguments(std::env::args_os()),
    };
    if let Err(reason) = reload.request(plan) {
        return CallToolResult::error_kind(
            ToolErrorKind::HandlerError {
                reason: reason.to_string(),
            },
            format!("reload_server could not queue the reload: {reason}"),
        );
    }

    CallToolResult::json(&json!({
        "reloading": true,
        "binary_path": executable.display().to_string(),
        "running_version": running_version,
        "candidate_version": disk_version,
        "arguments_preserved": true,
        "toolsets_reset": true,
    }))
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

async fn handle_get_installation_info(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let info = crate::runtime_info::collect(&ctx.config, &ctx.config_resolution).await;
    CallToolResult::json(&info)
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
mod reload_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;

    fn context() -> Arc<ToolContext> {
        Arc::new(ToolContext::new(
            ServerConfig::default(),
            Arc::new(ToolRouter::new()),
        ))
    }

    #[test]
    fn reload_registration_is_capability_gated() {
        assert!(!meta_tool_descriptions_for(false)
            .iter()
            .any(|tool| tool.name == "reload_server"));

        let enabled = meta_tool_descriptions_for(true);
        assert_eq!(
            enabled.iter().any(|tool| tool.name == "reload_server"),
            cfg!(unix)
        );
    }

    #[test]
    fn reload_control_is_a_one_shot_handoff() {
        let control = ReloadControl::default();
        assert!(!control.is_enabled());
        control.enable();
        assert!(control.is_enabled());

        let plan = ReloadPlan {
            binary_path: std::env::current_exe().unwrap(),
            #[cfg(unix)]
            executable: File::open(std::env::current_exe().unwrap()).unwrap(),
            #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
            identity: {
                let path = std::env::current_exe().unwrap();
                let file = File::open(&path).unwrap();
                ExecutableIdentity::capture(&file, &path).unwrap()
            },
            arguments: vec![OsString::from("--config"), OsString::from("custom.toml")],
        };
        control.request(plan).expect("first request queues");
        assert!(control
            .request(ReloadPlan {
                binary_path: std::env::current_exe().unwrap(),
                #[cfg(unix)]
                executable: File::open(std::env::current_exe().unwrap()).unwrap(),
                #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
                identity: {
                    let path = std::env::current_exe().unwrap();
                    let file = File::open(&path).unwrap();
                    ExecutableIdentity::capture(&file, &path).unwrap()
                },
                arguments: Vec::new(),
            })
            .is_err());
        let queued = control.take().expect("queued request");
        assert_eq!(queued.arguments[0], "--config");
        assert!(control.take().is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn an_open_executable_handle_can_be_invoked_after_its_path_is_irrelevant() {
        let directory = tempfile::tempdir().unwrap();
        let copied_path = directory.path().join("reload-candidate");
        std::fs::copy(std::env::current_exe().unwrap(), &copied_path).unwrap();
        let executable = File::open(&copied_path).unwrap();
        std::fs::remove_file(&copied_path).unwrap();
        let output = std::process::Command::new(executable_handle_path(&executable))
            .arg("--list")
            .output()
            .expect("the open test executable must be invokable through its descriptor");

        assert!(output.status.success());
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    #[test]
    fn a_non_linux_unix_handoff_refuses_a_replaced_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let copied_path = directory.path().join("reload-candidate");
        let current = std::env::current_exe().unwrap();
        std::fs::copy(&current, &copied_path).unwrap();
        let executable = File::open(&copied_path).unwrap();
        let identity = ExecutableIdentity::capture(&executable, &copied_path).unwrap();
        let plan = ReloadPlan {
            binary_path: copied_path.clone(),
            executable,
            identity,
            arguments: Vec::new(),
        };

        assert_eq!(plan.validated_exec_path().unwrap(), copied_path);
        std::fs::remove_file(&copied_path).unwrap();
        std::fs::copy(current, &copied_path).unwrap();
        assert!(plan.validated_exec_path().is_err());
    }

    #[test]
    fn reload_version_policy_covers_upgrade_equal_downgrade_and_unknown() {
        assert_eq!(validate_reload_version("0.12.0", "0.11.1", false), Ok(()));
        assert_eq!(
            validate_reload_version("0.11.0", "0.11.1", true),
            Err(ReloadVersionRefusal::Downgrade)
        );
        assert_eq!(
            validate_reload_version("0.11.1", "0.11.1", false),
            Err(ReloadVersionRefusal::SameVersionNeedsOptIn)
        );
        assert_eq!(validate_reload_version("0.11.1", "0.11.1", true), Ok(()));
        assert_eq!(
            validate_reload_version("development", "0.11.1", true),
            Err(ReloadVersionRefusal::Uncomparable)
        );
    }

    #[test]
    fn reload_preserves_every_original_argument_except_argv_zero() {
        let arguments = reload_arguments([
            OsString::from("konnect"),
            OsString::from("--config"),
            OsString::from("custom.toml"),
            OsString::from("--stdio"),
        ]);

        assert_eq!(
            arguments,
            ["--config", "custom.toml", "--stdio"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn reload_is_not_dispatched_when_disabled() {
        let control = ReloadControl::default();
        assert!(
            handle_meta_tool_with_reload("reload_server", &json!({}), &context(), &control)
                .await
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_requires_explicit_confirmation_before_any_probe() {
        let control = ReloadControl::default();
        control.enable();
        let result =
            handle_meta_tool_with_reload("reload_server", &json!({}), &context(), &control)
                .await
                .expect("enabled Unix reload dispatches");
        assert!(result.is_error);
        assert!(control.take().is_none());
    }
}
