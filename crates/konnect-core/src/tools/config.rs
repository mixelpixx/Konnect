//! `config` toolset — User preferences, project rules, and effective configuration.
//!
//! Persists user-level config to `~/.konnect/config.json` and project-level
//! config to `<project_dir>/.konnect/project.json`. Claude should call
//! `load_user_config` at the start of every session.
//!
//! A configuration file has four states and they are never merged (#580):
//! absent (the defaults, reported as such), loaded, malformed, and unreadable.
//! Only an absent file may read as the defaults. A file that exists and cannot
//! be used is a refusal, because the next save would otherwise persist those
//! defaults over the user's own settings.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{invalid_arg, require_str, ToolContext, ToolDef};
use konnect_sexp::writer::{write_atomic_if_unchanged, write_new_atomic};
use konnect_sexp::SexpError;
use serde_json::json;
use std::path::{Path, PathBuf};
use tracing::info;

// ─── Default config ──────────────────────────────────────────────────────────

fn default_user_config() -> serde_json::Value {
    json!({
        "preferred_manufacturers": [],
        "preferred_distributors": ["JLCPCB", "LCSC"],
        "default_passives": {
            "decoupling_cap": "100nF X7R 0402",
            "pull_up": "10k 0402",
            "bulk_cap": "10uF X5R 0805"
        },
        "fab_constraints": {
            "min_trace_width_mm": 0.15,
            "min_via_drill_mm": 0.3,
            "min_clearance_mm": 0.15,
            "layer_count": 2,
            "fab_house": "JLCPCB"
        },
        "naming_conventions": {
            "net_prefix_power": "VCC_",
            "net_prefix_ground": "GND"
        },
        "design_rules": [],
        // Sourcing policy consumed by the upcoming derating/AVL checks.
        // Derating limits are max operating/rated utilization — conservative
        // general practice, not MIL-HDBK-217. An empty AVL means "not
        // enforced", which those checks report as a warning, never a pass.
        "sourcing": {
            "avl": [],
            "derating": {
                "capacitor": { "voltage": 0.80 },
                "resistor": { "power": 0.60 },
                "inductor": { "current": 0.80 },
                "mosfet": { "vds": 0.80, "id": 0.80 },
                "diode": { "vr": 0.80, "if": 0.80 },
                "led": { "if": 0.80 },
                "connector": { "current": 0.80 },
                "regulator": { "power": 0.70, "current": 0.80 }
            }
        }
    })
}

fn default_project_config() -> serde_json::Value {
    json!({
        "design_rules": [],
        "fab_constraints": {},
        "naming_conventions": {},
        // Project-side sourcing overrides. NOTE: deep_merge REPLACES arrays,
        // so a project-level "avl" supersedes the user list entirely rather
        // than appending to it — the same semantics design_rules has.
        "sourcing": {}
    })
}

// ─── Config file paths ───────────────────────────────────────────────────────

fn user_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("konnect")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("konnect")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".konnect")
    }
}

fn user_config_path() -> PathBuf {
    user_config_dir().join("config.json")
}

fn project_config_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".konnect").join("project.json")
}

// ─── Config I/O helpers ──────────────────────────────────────────────────────

/// What a configuration file on disk turned out to be.
#[derive(Debug)]
enum ConfigRead {
    /// No file. The one state that may read as the defaults.
    Absent,
    /// A JSON object, with the exact text a conditional save must still find.
    Loaded {
        value: serde_json::Value,
        text: String,
    },
    /// The file exists and is not a JSON object.
    Malformed { reason: String },
    /// The file exists, or may, and could not be read: permissions, a
    /// directory at the path, invalid UTF-8.
    Unreadable { reason: String },
}

fn read_config(path: &Path) -> ConfigRead {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ConfigRead::Absent,
        Err(error) => {
            return ConfigRead::Unreadable {
                reason: error.to_string(),
            }
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) if value.is_object() => ConfigRead::Loaded { value, text },
        Ok(other) => ConfigRead::Malformed {
            reason: format!(
                "the document root is {}, not an object",
                json_type_name(&other)
            ),
        },
        Err(error) => ConfigRead::Malformed {
            reason: error.to_string(),
        },
    }
}

/// Where a reported configuration came from.
const FROM_FILE: &str = "file";
const FROM_DEFAULTS: &str = "defaults";
/// No project directory was given or configured, so there is no project file
/// to read and the project half is the defaults.
const FROM_NO_PROJECT: &str = "not_configured";

/// A configuration to work from: the document, its origin, and the exact text
/// on disk when there is one.
struct UsableConfig {
    value: serde_json::Value,
    source: &'static str,
    text: Option<String>,
}

fn usable_config(
    path: &Path,
    which: &str,
    default: serde_json::Value,
) -> Result<UsableConfig, CallToolResult> {
    match read_config(path) {
        ConfigRead::Absent => Ok(UsableConfig {
            value: default,
            source: FROM_DEFAULTS,
            text: None,
        }),
        ConfigRead::Loaded { value, text } => Ok(UsableConfig {
            value,
            source: FROM_FILE,
            text: Some(text),
        }),
        ConfigRead::Malformed { reason } => Err(unusable_config(
            path,
            which,
            format!("malformed_json: {reason}"),
        )),
        ConfigRead::Unreadable { reason } => Err(unusable_config(
            path,
            which,
            format!("unreadable: {reason}"),
        )),
    }
}

/// The refusal for a configuration file that exists and cannot be used.
fn unusable_config(path: &Path, which: &str, reason: String) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::InvalidConfiguration {
            path: path.display().to_string(),
            reason: reason.clone(),
        },
        format!(
            "The {which} configuration at '{}' exists but cannot be used ({reason}). Konnect did \
             not substitute defaults and wrote nothing, because saving over it would replace \
             your settings. Repair or move the file, then retry.",
            path.display()
        ),
    )
}

/// Create the file when none was read, replace it only if it still holds the
/// text that was read.
fn persist_config(path: &Path, expected: Option<&str>, content: &str) -> Result<(), SexpError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match expected {
        Some(expected) => write_atomic_if_unchanged(path, expected, content),
        None => write_new_atomic(path, content),
    }
}

/// What [`update_config`] committed, read back from disk.
struct SavedConfig {
    value: serde_json::Value,
    created: bool,
}

/// Apply `change` to one configuration file and persist it.
///
/// Persistence is injectable so a test can fail it after a real replacement
/// without racing an editor; production passes [`persist_config`].
fn update_config(
    path: &Path,
    which: &str,
    default: serde_json::Value,
    operation: &str,
    change: impl FnOnce(&mut serde_json::Value) -> Result<(), CallToolResult>,
    persist: impl FnOnce(&Path, Option<&str>, &str) -> Result<(), SexpError>,
) -> Result<SavedConfig, CallToolResult> {
    let UsableConfig {
        mut value, text, ..
    } = usable_config(path, which, default)?;
    change(&mut value)?;
    let content = serde_json::to_string_pretty(&value)
        .map_err(|error| CallToolResult::error(format!("{operation}: {error}")))?;
    let created = text.is_none();

    match persist(path, text.as_deref(), &content) {
        Ok(()) => {}
        // An atomic no-clobber create that finds a file never replaced
        // anything: that is positive evidence nothing was written.
        Err(SexpError::Io(error))
            if created && error.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            return Err(CallToolResult::error_kind(
                ToolErrorKind::Conflict {
                    paths: vec![path.display().to_string()],
                },
                format!(
                    "A {which} configuration appeared at '{}' while it was being created; \
                     nothing was written. Retry to update the file that is there now.",
                    path.display()
                ),
            ));
        }
        // Everything else proves nothing about the disk: the conditional
        // writer returns `Conflict` both before a replacement and after one
        // whose readback differs (#579), and an I/O error can follow either.
        Err(error) => {
            return Err(super::file_outcome_uncertain(
                path,
                operation,
                error.to_string(),
            ))
        }
    }

    match read_config(path) {
        ConfigRead::Loaded {
            value: observed, ..
        } if observed == value => Ok(SavedConfig {
            value: observed,
            created,
        }),
        other => Err(super::file_outcome_uncertain(
            path,
            operation,
            format!("the saved file did not read back as written ({other:?})"),
        )),
    }
}

/// The `change` that sets one dot-path key; a path through a non-object is
/// the caller's argument, not an internal failure.
fn set_key(
    key_path: &str,
    value: serde_json::Value,
) -> impl FnOnce(&mut serde_json::Value) -> Result<(), CallToolResult> + '_ {
    move |config| {
        set_dot_path(config, key_path, value)
            .map_err(|error| invalid_arg("key_path", &error.to_string()))
    }
}

/// The `change` that appends one design rule.
fn push_rule(rule: &str) -> impl FnOnce(&mut serde_json::Value) -> Result<(), CallToolResult> + '_ {
    move |config| {
        match config["design_rules"].as_array_mut() {
            Some(rules) => rules.push(json!(rule)),
            None => config["design_rules"] = json!([rule]),
        }
        Ok(())
    }
}

fn design_rules(config: &serde_json::Value) -> Vec<String> {
    config["design_rules"]
        .as_array()
        .map(|rules| {
            rules
                .iter()
                .filter_map(|rule| rule.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Deep merge: overlay values onto base. overlay takes precedence.
fn deep_merge(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            let mut merged = b.clone();
            for (key, val) in o {
                let base_val = merged.get(key).cloned().unwrap_or(serde_json::Value::Null);
                merged.insert(key.clone(), deep_merge(&base_val, val));
            }
            serde_json::Value::Object(merged)
        }
        (_, overlay) if !overlay.is_null() => overlay.clone(),
        (base, _) => base.clone(),
    }
}

/// Set a value at a dot-notation path, e.g. "fab_constraints.fab_house" = "JLCPCB".
///
/// Fails with an error (instead of panicking) if a segment of the path already
/// holds a non-object value, since there is nowhere to insert the child key.
fn set_dot_path(
    config: &mut serde_json::Value,
    key_path: &str,
    value: serde_json::Value,
) -> anyhow::Result<()> {
    let parts: Vec<&str> = key_path.split('.').collect();
    let mut current = config;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Last part — set the value
            return match current {
                serde_json::Value::Object(map) => {
                    map.insert(part.to_string(), value);
                    Ok(())
                }
                other => anyhow::bail!(
                    "Cannot set '{key_path}': '{}' is not an object (found {})",
                    parts[..i].join("."),
                    json_type_name(other)
                ),
            };
        }
        // Navigate into nested object, creating it if missing.
        if !current.get(*part).map(|v| v.is_object()).unwrap_or(false) {
            match current {
                serde_json::Value::Object(map) => {
                    map.insert(part.to_string(), json!({}));
                }
                other => anyhow::bail!(
                    "Cannot set '{key_path}': '{}' is not an object (found {})",
                    parts[..i].join("."),
                    json_type_name(other)
                ),
            }
        }
        current = current
            .get_mut(*part)
            .expect("just verified or inserted as an object above");
    }
    Ok(())
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "load_user_config",
            "Load the user's global Konnect preferences. Call this at the start of every session \
             to get preferred manufacturers, fab constraints, default passives, and design rules. \
             'source' says whether the answer is the user's file or Konnect's defaults (no file \
             existed; the defaults are then written for the user to edit and 'persisted' says \
             whether that worked). A file that exists but cannot be parsed or read is refused \
             with invalid_configuration and is never replaced by defaults.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_load_user_config(args, ctx).await }
        ),
        tool!(
            "save_user_config",
            "Update a user preference. Use dot-notation for nested keys, e.g. 'fab_constraints.fab_house'. \
             Call this when the user says things like 'always use JLCPCB' or 'I prefer 0402 passives'. \
             Every other key is kept, and the response is the file as read back. Refuses with \
             invalid_configuration, writing nothing, when the existing file cannot be parsed or \
             read; a persistence failure is mutation_outcome_uncertain, so read the file before retrying.",
            json!({
                "type": "object",
                "properties": {
                    "key_path": {
                        "type": "string",
                        "description": "Dot-notation path to the config key, e.g. 'fab_constraints.fab_house' or 'default_passives.decoupling_cap'"
                    },
                    "value": {
                        "description": "New value to set (string, number, array, or object)"
                    }
                },
                "required": ["key_path", "value"]
            }),
            |args, ctx| async move { handle_save_user_config(args, ctx).await }
        ),
        tool!(
            "load_project_config",
            "Load project-specific configuration from <project_dir>/.konnect/project.json. \
             Project config overrides user config where both exist. 'source' is 'file' or \
             'defaults' (no file; nothing is written). A file that cannot be parsed or read is \
             refused with invalid_configuration.",
            json!({
                "type": "object",
                "properties": {
                    "project_dir": {
                        "type": "string",
                        "description": "Path to the KiCAD project directory. If omitted, uses the configured project_dir."
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_load_project_config(args, ctx).await }
        ),
        tool!(
            "save_project_config",
            "Save a project-specific rule or override. Same dot-notation as save_user_config \
             but writes to the project's .konnect/project.json, with the same refusals: an \
             unusable existing file is never overwritten.",
            json!({
                "type": "object",
                "properties": {
                    "project_dir": { "type": "string", "description": "Project directory (optional, uses configured default)" },
                    "key_path": { "type": "string", "description": "Dot-notation config key" },
                    "value": { "description": "New value to set" }
                },
                "required": ["key_path", "value"]
            }),
            |args, ctx| async move { handle_save_project_config(args, ctx).await }
        ),
        tool!(
            "get_effective_config",
            "Return the merged configuration (user defaults + project overrides). \
             This is the config Claude should use for all design decisions. 'sources' says \
             where each half came from ('file', 'defaults', or 'not_configured' when there is no \
             project directory). Refuses with invalid_configuration, naming which file, rather \
             than merging defaults in place of a file that cannot be used.",
            json!({
                "type": "object",
                "properties": {
                    "project_dir": { "type": "string", "description": "Project directory (optional)" }
                },
                "required": []
            }),
            |args, ctx| async move { handle_get_effective_config(args, ctx).await }
        ),
        tool!(
            "add_design_rule",
            "Add a natural-language design rule that Claude should follow in this project. \
             Examples: 'Always use 100nF X7R for MCU decoupling within 3mm of power pin', \
             'Route USB D+/D- as 90-ohm differential pair'.",
            json!({
                "type": "object",
                "properties": {
                    "rule": { "type": "string", "description": "The design rule in plain English" },
                    "scope": {
                        "type": "string",
                        "description": "'user' (applies to all projects) or 'project' (this project only)",
                        "default": "project"
                    },
                    "project_dir": { "type": "string", "description": "Project directory (for project-scoped rules)" }
                },
                "required": ["rule"]
            }),
            |args, ctx| async move { handle_add_design_rule(args, ctx).await }
        ),
        tool!(
            "list_design_rules",
            "List all active design rules (user-level + project-level).",
            json!({
                "type": "object",
                "properties": {
                    "project_dir": { "type": "string", "description": "Project directory (optional)" }
                },
                "required": []
            }),
            |args, ctx| async move { handle_list_design_rules(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────
//
// Each handler resolves its paths and hands them to a synchronous core, so the
// cores can be driven against a temporary directory without touching the
// developer's own preferences.

fn load_user_config_at(path: &Path) -> CallToolResult {
    let config = match usable_config(path, "user", default_user_config()) {
        Ok(config) => config,
        Err(refusal) => return refusal,
    };
    let mut body = json!({
        "config": config.value,
        "path": path.to_str().unwrap_or(""),
        "source": config.source,
    });
    if config.source == FROM_DEFAULTS {
        // First run: leave the user a file to edit. A failure here is
        // reported, not swallowed; the defaults are still the answer.
        let content = serde_json::to_string_pretty(&body["config"]).unwrap_or_default();
        match persist_config(path, None, &content) {
            Ok(()) => body["persisted"] = json!(true),
            Err(error) => {
                body["persisted"] = json!(false);
                body["persist_error"] = json!(error.to_string());
            }
        }
        body["note"] = json!(
            "No user preferences file existed, so these are Konnect's defaults. Project config \
             may override these values."
        );
    } else {
        body["note"] = json!(
            "User preferences loaded from the file. Project config may override these values."
        );
    }
    CallToolResult::json(&body)
}

async fn handle_load_user_config(
    _args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = user_config_path();
    info!(path = %path.display(), "[BETA] Loading user config");
    Ok(load_user_config_at(&path))
}

fn required_value(args: &serde_json::Value) -> Result<serde_json::Value, CallToolResult> {
    let value = args["value"].clone();
    if value.is_null() {
        return Err(CallToolResult::error("Missing required argument: 'value'"));
    }
    Ok(value)
}

async fn handle_save_user_config(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let key_path = match require_str(args, "key_path") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match required_value(args) {
        Ok(value) => value,
        Err(refusal) => return Ok(refusal),
    };

    let path = user_config_path();
    info!(key_path = %key_path, "[BETA] Saving user config");
    Ok(
        match update_config(
            &path,
            "user",
            default_user_config(),
            "save_user_config",
            set_key(&key_path, value.clone()),
            persist_config,
        ) {
            Ok(saved) => CallToolResult::json(&json!({
                "updated": key_path,
                "value": value,
                "config": saved.value,
                "path": path.to_str().unwrap_or(""),
                "created": saved.created,
            })),
            Err(refusal) => refusal,
        },
    )
}

async fn handle_load_project_config(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let project_dir = resolve_project_dir(args, ctx)?;
    let path = project_config_path(&project_dir);
    info!(path = %path.display(), "[BETA] Loading project config");
    Ok(
        match usable_config(&path, "project", default_project_config()) {
            Ok(config) => CallToolResult::json(&json!({
                "config": config.value,
                "project_dir": project_dir.to_str().unwrap_or(""),
                "path": path.to_str().unwrap_or(""),
                "source": config.source,
            })),
            Err(refusal) => refusal,
        },
    )
}

async fn handle_save_project_config(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let project_dir = resolve_project_dir(args, ctx)?;
    let key_path = match require_str(args, "key_path") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match required_value(args) {
        Ok(value) => value,
        Err(refusal) => return Ok(refusal),
    };

    let path = project_config_path(&project_dir);
    Ok(
        match update_config(
            &path,
            "project",
            default_project_config(),
            "save_project_config",
            set_key(&key_path, value.clone()),
            persist_config,
        ) {
            Ok(saved) => CallToolResult::json(&json!({
                "updated": key_path,
                "value": value,
                "project_dir": project_dir.to_str().unwrap_or(""),
                "path": path.to_str().unwrap_or(""),
                "created": saved.created,
            })),
            Err(refusal) => refusal,
        },
    )
}

/// The user and project documents both tools below read, or the refusal for
/// whichever of them cannot be used.
fn both_configs(
    user_path: &Path,
    project_path: Option<&Path>,
) -> Result<(UsableConfig, UsableConfig), CallToolResult> {
    let user = usable_config(user_path, "user", default_user_config())?;
    let project = match project_path {
        Some(path) => usable_config(path, "project", default_project_config())?,
        None => UsableConfig {
            value: default_project_config(),
            source: FROM_NO_PROJECT,
            text: None,
        },
    };
    Ok((user, project))
}

fn effective_config_at(user_path: &Path, project_path: Option<&Path>) -> CallToolResult {
    let (user, project) = match both_configs(user_path, project_path) {
        Ok(configs) => configs,
        Err(refusal) => return refusal,
    };
    CallToolResult::json(&json!({
        "effective_config": deep_merge(&user.value, &project.value),
        "sources": { "user": user.source, "project": project.source },
        "note": "Merged user defaults + project overrides. Use these values for all design decisions."
    }))
}

async fn handle_get_effective_config(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let project_path = resolve_project_dir(args, ctx)
        .ok()
        .map(|dir| project_config_path(&dir));
    Ok(effective_config_at(
        &user_config_path(),
        project_path.as_deref(),
    ))
}

async fn handle_add_design_rule(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let rule = match require_str(args, "rule") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let scope = args["scope"].as_str().unwrap_or("project");

    let (path, which, default) = if scope == "user" {
        (user_config_path(), "user", default_user_config())
    } else {
        let project_dir = resolve_project_dir(args, ctx)?;
        (
            project_config_path(&project_dir),
            "project",
            default_project_config(),
        )
    };
    Ok(
        match update_config(
            &path,
            which,
            default,
            "add_design_rule",
            push_rule(&rule),
            persist_config,
        ) {
            Ok(saved) => CallToolResult::json(&json!({
                "added_rule": rule,
                "scope": scope,
                "path": path.to_str().unwrap_or(""),
                "created": saved.created,
            })),
            Err(refusal) => refusal,
        },
    )
}

fn list_design_rules_at(user_path: &Path, project_path: Option<&Path>) -> CallToolResult {
    let (user, project) = match both_configs(user_path, project_path) {
        Ok(configs) => configs,
        Err(refusal) => return refusal,
    };
    let user_rules = design_rules(&user.value);
    let project_rules = design_rules(&project.value);
    CallToolResult::json(&json!({
        "user_rules": user_rules,
        "project_rules": project_rules,
        "total": user_rules.len() + project_rules.len(),
        "sources": { "user": user.source, "project": project.source },
    }))
}

async fn handle_list_design_rules(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let project_path = resolve_project_dir(args, ctx)
        .ok()
        .map(|dir| project_config_path(&dir));
    Ok(list_design_rules_at(
        &user_config_path(),
        project_path.as_deref(),
    ))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn resolve_project_dir(args: &serde_json::Value, ctx: &ToolContext) -> anyhow::Result<PathBuf> {
    if let Some(dir) = args["project_dir"].as_str() {
        return Ok(PathBuf::from(dir));
    }
    if let Some(ref dir) = ctx.config.project_dir {
        return Ok(dir.clone());
    }
    anyhow::bail!("No project directory specified. Pass 'project_dir' or configure a default.")
}

#[cfg(test)]
mod dot_path_and_merge_tests {
    use super::*;

    #[test]
    fn deep_merge_overlays_nested_object_keys() {
        let base = json!({
            "fab_constraints": { "fab_house": "JLCPCB", "layer_count": 2 },
            "design_rules": []
        });
        let overlay = json!({
            "fab_constraints": { "layer_count": 4 }
        });

        let merged = deep_merge(&base, &overlay);

        assert_eq!(merged["fab_constraints"]["fab_house"], "JLCPCB");
        assert_eq!(merged["fab_constraints"]["layer_count"], 4);
        assert_eq!(merged["design_rules"], json!([]));
    }

    /// The sourcing policy the derating/AVL checks read must exist in the
    /// defaults with the documented limits — a missing key would make those
    /// checks silently unenforceable, the #218 class.
    #[test]
    fn default_sourcing_policy_is_present_with_conservative_limits() {
        let user = default_user_config();
        assert_eq!(user["sourcing"]["avl"], json!([]));
        assert_eq!(user["sourcing"]["derating"]["capacitor"]["voltage"], 0.80);
        assert_eq!(user["sourcing"]["derating"]["resistor"]["power"], 0.60);
        assert_eq!(user["sourcing"]["derating"]["regulator"]["power"], 0.70);
        assert_eq!(default_project_config()["sourcing"], json!({}));
    }

    /// A project-level AVL REPLACES the user list (deep_merge array
    /// semantics) — pinned so a future "append" change is a decision, not
    /// an accident.
    #[test]
    fn project_avl_replaces_user_avl_wholesale() {
        let user = json!({ "sourcing": { "avl": ["Murata", "TDK"] } });
        let project = json!({ "sourcing": { "avl": ["Vishay"] } });
        let merged = deep_merge(&user, &project);
        assert_eq!(merged["sourcing"]["avl"], json!(["Vishay"]));
    }

    #[test]
    fn deep_merge_null_overlay_value_keeps_base() {
        let base = json!({ "fab_house": "JLCPCB" });
        let overlay = json!({ "fab_house": null });

        let merged = deep_merge(&base, &overlay);

        assert_eq!(merged["fab_house"], "JLCPCB");
    }

    #[test]
    fn set_dot_path_sets_top_level_key() {
        let mut config = json!({});
        set_dot_path(&mut config, "fab_house", json!("JLCPCB")).expect("should succeed");
        assert_eq!(config["fab_house"], "JLCPCB");
    }

    #[test]
    fn set_dot_path_creates_missing_intermediate_objects() {
        let mut config = json!({});
        set_dot_path(&mut config, "fab_constraints.fab_house", json!("JLCPCB"))
            .expect("should succeed");
        assert_eq!(config["fab_constraints"]["fab_house"], "JLCPCB");
    }

    #[test]
    fn set_dot_path_overwrites_existing_nested_value() {
        let mut config = json!({ "fab_constraints": { "fab_house": "PCBWay" } });
        set_dot_path(&mut config, "fab_constraints.fab_house", json!("JLCPCB"))
            .expect("should succeed");
        assert_eq!(config["fab_constraints"]["fab_house"], "JLCPCB");
    }

    #[test]
    fn set_dot_path_errors_instead_of_panicking_on_non_object_root() {
        // Regression test: a corrupted config file that parses as valid JSON
        // but isn't a `{...}` object used to make this function panic via
        // `.unwrap()` on a failed `get_mut`, crashing the whole server.
        let mut config = json!(null);
        let result = set_dot_path(&mut config, "fab_constraints.fab_house", json!("JLCPCB"));
        assert!(result.is_err());
    }

    #[test]
    fn set_dot_path_replaces_scalar_intermediate_segment_with_object() {
        // "fab_constraints" already holds a string, not an object. The parent
        // (root) is still an object, so it's free to replace that key with a
        // fresh nested object rather than erroring — this matches the
        // function's pre-existing "create if needed" behavior.
        let mut config = json!({ "fab_constraints": "JLCPCB" });
        set_dot_path(&mut config, "fab_constraints.fab_house", json!("PCBWay"))
            .expect("should succeed by replacing the scalar with an object");
        assert_eq!(config["fab_constraints"]["fab_house"], "PCBWay");
    }
}

#[cfg(test)]
mod config_state_tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use crate::mcp::protocol::ToolContent;

    /// A truncated document that still visibly holds the user's settings.
    const TRUNCATED: &str =
        r#"{"kicad_cli": "D:/tools/kicad-cli.exe", "sourcing": {"avl": ["Vishay", "Murata"]},"#;

    fn body(result: &CallToolResult) -> serde_json::Value {
        match result.content.first() {
            Some(ToolContent::Text { text }) => serde_json::from_str(text).unwrap(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn text(result: &CallToolResult) -> String {
        match result.content.first() {
            Some(ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn config_in(dir: &Path) -> PathBuf {
        dir.join("konnect").join("config.json")
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn the_four_read_states_stay_apart() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        assert!(matches!(read_config(&path), ConfigRead::Absent));

        write(&path, r#"{"a": 1}"#);
        assert!(matches!(read_config(&path), ConfigRead::Loaded { .. }));

        write(&path, TRUNCATED);
        assert!(matches!(read_config(&path), ConfigRead::Malformed { .. }));

        write(&path, "[1, 2]");
        let ConfigRead::Malformed { reason } = read_config(&path) else {
            panic!("a non-object root is not a configuration")
        };
        assert!(reason.contains("array"), "{reason}");

        // A directory where the file should be: present, and not readable.
        let blocked = dir.path().join("blocked").join("config.json");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(matches!(
            read_config(&blocked),
            ConfigRead::Unreadable { .. }
        ));
    }

    #[test]
    fn an_absent_file_answers_with_the_defaults_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        let result = load_user_config_at(&path);
        assert!(!result.is_error);
        let body = body(&result);
        assert_eq!(body["source"], "defaults", "{body}");
        assert_eq!(body["persisted"], true, "{body}");
        assert_eq!(body["config"], default_user_config());
        assert!(
            matches!(read_config(&path), ConfigRead::Loaded { .. }),
            "the defaults were left on disk for the user to edit"
        );

        let again = body_of_loaded(&path);
        assert_eq!(again["source"], "file", "{again}");
        assert!(again.get("persisted").is_none(), "{again}");
    }

    fn body_of_loaded(path: &Path) -> serde_json::Value {
        body(&load_user_config_at(path))
    }

    #[test]
    fn a_failed_first_persistence_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        // The parent of the config directory is a file, so it cannot be made.
        let blocker = dir.path().join("konnect");
        std::fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("config.json");

        let result = load_user_config_at(&path);
        // `konnect/config.json` under a *file* reads as unreadable on some
        // platforms and absent on others; both are truthful, neither is "loaded".
        if result.is_error {
            assert_eq!(
                extract_error_kind(&result).as_deref(),
                Some("invalid_configuration")
            );
            return;
        }
        let body = body(&result);
        assert_eq!(body["source"], "defaults", "{body}");
        assert_eq!(body["persisted"], false, "{body}");
        assert!(body["persist_error"].is_string(), "{body}");
    }

    #[test]
    fn a_malformed_file_is_refused_by_load_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        write(&path, TRUNCATED);

        let result = load_user_config_at(&path);
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("invalid_configuration")
        );
        let body = body(&result);
        assert!(
            body["error"]["reason"]
                .as_str()
                .unwrap()
                .starts_with("malformed_json:"),
            "{body}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TRUNCATED);
    }

    #[test]
    fn a_save_over_a_malformed_file_is_refused_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        write(&path, TRUNCATED);

        let refusal = update_config(
            &path,
            "user",
            default_user_config(),
            "save_user_config",
            set_key("ui.theme", json!("dark")),
            persist_config,
        )
        .err()
        .expect("a save after a failed read must refuse");
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("invalid_configuration")
        );
        assert!(text(&refusal).contains("wrote nothing"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            TRUNCATED,
            "the user's settings are still there to repair"
        );
    }

    #[test]
    fn a_save_keeps_every_other_key_and_reports_the_readback() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        write(
            &path,
            r#"{"kicad_cli": "D:/tools/kicad-cli.exe", "ui": {"density": "compact"}}"#,
        );

        let saved = update_config(
            &path,
            "user",
            default_user_config(),
            "save_user_config",
            set_key("ui.theme", json!("dark")),
            persist_config,
        )
        .expect("a usable file is updated");
        assert!(!saved.created);
        assert_eq!(saved.value["kicad_cli"], "D:/tools/kicad-cli.exe");
        assert_eq!(saved.value["ui"]["density"], "compact");
        assert_eq!(saved.value["ui"]["theme"], "dark");
        let ConfigRead::Loaded { value, .. } = read_config(&path) else {
            panic!("the saved file parses")
        };
        assert_eq!(value, saved.value, "the response is the committed file");
    }

    #[test]
    fn a_save_with_no_file_creates_one_from_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        let saved = update_config(
            &path,
            "user",
            default_user_config(),
            "save_user_config",
            set_key("ui.theme", json!("dark")),
            persist_config,
        )
        .expect("an absent file is created");
        assert!(saved.created);
        let mut expected = default_user_config();
        expected["ui"] = json!({ "theme": "dark" });
        assert_eq!(saved.value, expected);
    }

    /// The conditional writer returns `Conflict` both before a replacement and
    /// after one whose readback differs, so neither timing may be reported as
    /// a no-write refusal (#579).
    #[test]
    fn persistence_conflicts_require_inspection_before_retry() {
        for after_replacement in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = config_in(dir.path());
            let original = r#"{"ui": {"density": "compact"}}"#;
            write(&path, original);

            let refusal = update_config(
                &path,
                "user",
                default_user_config(),
                "save_user_config",
                set_key("ui.theme", json!("dark")),
                |target, expected, content| {
                    if after_replacement {
                        persist_config(target, expected, content).unwrap();
                    }
                    Err(SexpError::Conflict {
                        path: target.to_path_buf(),
                    })
                },
            )
            .err()
            .expect("a persistence conflict is not a success");

            assert_eq!(
                extract_error_kind(&refusal).as_deref(),
                Some("mutation_outcome_uncertain")
            );
            let message = text(&refusal);
            assert!(!message.contains("nothing was written"), "{message}");
            assert!(message.contains("read it before retrying"), "{message}");
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(on_disk == original, !after_replacement);
        }
    }

    /// Creating a file that turns out to exist never replaced anything, so
    /// that one case may say nothing was written.
    #[test]
    fn a_file_that_appears_during_creation_is_a_no_write_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        let refusal = update_config(
            &path,
            "user",
            default_user_config(),
            "save_user_config",
            set_key("ui.theme", json!("dark")),
            |_, expected, _| {
                assert!(expected.is_none(), "no file was read, so none is expected");
                Err(SexpError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "appeared",
                )))
            },
        )
        .err()
        .expect("the race is reported");
        assert_eq!(extract_error_kind(&refusal).as_deref(), Some("conflict"));
        assert!(text(&refusal).contains("nothing was written"));
    }

    #[test]
    fn a_readback_that_differs_is_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_in(dir.path());
        write(&path, r#"{"a": 1}"#);
        let refusal = update_config(
            &path,
            "user",
            default_user_config(),
            "save_user_config",
            set_key("b", json!(2)),
            |target, _, _| {
                std::fs::write(target, r#"{"someone": "else"}"#).unwrap();
                Ok(())
            },
        )
        .err()
        .expect("a file that does not show the change is not a success");
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("mutation_outcome_uncertain")
        );
    }

    #[test]
    fn either_unusable_file_refuses_the_merged_view_and_names_which() {
        let dir = tempfile::tempdir().unwrap();
        let user = config_in(dir.path());
        let project = dir
            .path()
            .join("proj")
            .join(".konnect")
            .join("project.json");

        let clean = body(&effective_config_at(&user, Some(&project)));
        assert_eq!(
            clean["sources"],
            json!({ "user": "defaults", "project": "defaults" })
        );
        let no_project = body(&effective_config_at(&user, None));
        assert_eq!(no_project["sources"]["project"], "not_configured");

        write(&project, TRUNCATED);
        let refusal = effective_config_at(&user, Some(&project));
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("invalid_configuration")
        );
        assert!(text(&refusal).contains("project configuration"));
        let rules = list_design_rules_at(&user, Some(&project));
        assert_eq!(
            extract_error_kind(&rules).as_deref(),
            Some("invalid_configuration")
        );

        std::fs::remove_file(&project).unwrap();
        write(&user, TRUNCATED);
        let refusal = effective_config_at(&user, Some(&project));
        assert!(text(&refusal).contains("user configuration"));
    }

    #[test]
    fn a_design_rule_is_appended_and_never_over_a_malformed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".konnect").join("project.json");
        let saved = update_config(
            &path,
            "project",
            default_project_config(),
            "add_design_rule",
            push_rule("Route USB as a 90-ohm pair"),
            persist_config,
        )
        .expect("the update succeeds");
        assert_eq!(
            design_rules(&saved.value),
            vec!["Route USB as a 90-ohm pair".to_string()]
        );

        write(&path, TRUNCATED);
        let refusal = update_config(
            &path,
            "project",
            default_project_config(),
            "add_design_rule",
            push_rule("second"),
            persist_config,
        )
        .err()
        .unwrap();
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("invalid_configuration")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TRUNCATED);
    }
}
