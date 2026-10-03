//! MCP protocol tests over stdio — spawn the real binary and speak JSON-RPC.
//!
//! Codifies the smoke tests that were run by hand at release time: handshake,
//! toolset loading for the entire registry, a real file-based tool call, and
//! the structured-error taxonomy the LLM relies on for recovery.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
    /// The handshake's `initialize` response.
    initialized: Value,
}

impl McpProcess {
    fn spawn() -> Self {
        Self::spawn_in_dir(None)
    }

    /// Spawn with the process working directory set to `dir`, so
    /// the config search's first path (`konnect.toml` in cwd) picks up
    /// a test config file placed there.
    fn spawn_in_dir(dir: Option<&std::path::Path>) -> Self {
        Self::spawn_configured(dir, false)
    }

    /// Spawn with the working directory set to `dir` and, when `isolate_home`,
    /// the platform config directory redirected there too.
    ///
    /// `dirs_config_path()` is the last search candidate and is built from
    /// `HOME` (`APPDATA` on Windows), so on a developer machine that already has
    /// `~/Library/Application Support/konnect/config.toml` a "no config exists"
    /// test silently resolves to that real file instead. Redirecting HOME makes
    /// the candidate list depend only on the temp dir.
    fn spawn_configured(dir: Option<&std::path::Path>, isolate_home: bool) -> Self {
        Self::spawn_with_env(dir, isolate_home, &[])
    }

    /// As `spawn_configured`, with extra environment variables for the child.
    /// Set on the child rather than this process because `std::env::set_var` is
    /// process-wide and the unit tests run in parallel — doing it in-process
    /// raced the #39 env-fallback test.
    fn spawn_with_env(
        dir: Option<&std::path::Path>,
        isolate_home: bool,
        env: &[(&str, &str)],
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_konnect"));
        for (key, value) in env {
            command.env(key, value);
        }
        if let Some(dir) = dir {
            command.current_dir(dir);
            if isolate_home {
                command.env("HOME", dir);
                command.env("APPDATA", dir);
            }
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn konnect binary");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut p = McpProcess {
            child,
            stdin,
            reader,
            next_id: 1,
            initialized: Value::Null,
        };
        // MCP handshake
        let init = p.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "protocol-test", "version": "0"}
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "konnect");
        p.initialized = init;
        p.notify("notifications/initialized");
        p
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
        // Read lines until the response with our id arrives (skips any
        // notifications the server might emit).
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(
                n > 0,
                "server closed stdout waiting for response to {method}"
            );
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", msg).unwrap();
        self.stdin.flush().unwrap();
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": name, "arguments": args}));
        resp["result"].clone()
    }

    /// Send a `tools/call`, then a fencing `ping`, and return every line the
    /// server emits up to and including the ping response. The fence
    /// guarantees the read loop terminates even when the tool call emits no
    /// notification (as in bug #19), so a test can assert on side-effect
    /// notifications without risking a hang.
    fn call_tool_then_fence(&mut self, name: &str, args: Value) -> Vec<Value> {
        let call_id = self.next_id;
        self.next_id += 1;
        let call = json!({
            "jsonrpc": "2.0", "id": call_id, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        });
        writeln!(self.stdin, "{}", call).unwrap();
        let fence_id = self.next_id;
        self.next_id += 1;
        let fence = json!({"jsonrpc": "2.0", "id": fence_id, "method": "ping", "params": {}});
        writeln!(self.stdin, "{}", fence).unwrap();
        self.stdin.flush().unwrap();

        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "server closed stdout before fence response");
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            let is_fence = v.get("id").and_then(Value::as_i64) == Some(fence_id);
            lines.push(v);
            if is_fence {
                break;
            }
        }
        lines
    }

    /// Parse the JSON body of a tool result's first text content.
    fn tool_body(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().unwrap_or("{}");
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_baseline_and_full_registry_loads() {
    let mut p = McpProcess::spawn();

    // Baseline tools/list: starter kit + meta-tools only (small context).
    let list = p.request("tools/list", json!({}));
    let baseline = list["result"]["tools"].as_array().unwrap().len();
    assert!(
        (10..30).contains(&baseline),
        "baseline tools/list should be the small starter kit, got {baseline}"
    );

    // list_toolboxes reports the registry; every toolset must load.
    let boxes = McpProcess::tool_body(&p.call_tool("list_toolboxes", json!({})));
    let toolsets: Vec<String> = boxes["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        toolsets.len() >= 17,
        "expected 17+ toolsets, got {}",
        toolsets.len()
    );
    // No license-era fields may reappear.
    assert!(boxes.get("license_tier").is_none());
    assert!(boxes["toolsets"][0].get("tier").is_none());

    let mut total = 0u64;
    for name in &toolsets {
        let loaded = McpProcess::tool_body(&p.call_tool("load_toolset", json!({"name": name})));
        let added = loaded["tools_added"].as_u64().unwrap_or(0);
        assert!(added > 0, "toolset '{name}' loaded no tools");
        total += added;
    }
    assert_eq!(
        total,
        boxes["total_tools"].as_u64().unwrap(),
        "sum of loaded tools disagrees with list_toolboxes total"
    );
}

#[test]
fn installation_info_reports_the_serving_process_without_leaking_endpoint_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("konnect.toml"),
        "ipc_address = \"ipc://diagnostic-test.sock?token=secret#fragment\"\n",
    )
    .unwrap();
    let mut p = McpProcess::spawn_in_dir(Some(tmp.path()));

    let list = p.request("tools/list", json!({}));
    assert!(list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "get_installation_info"));

    let result = p.call_tool("get_installation_info", json!({}));
    assert_ne!(result["isError"], json!(true), "{result:#?}");
    let body = McpProcess::tool_body(&result);

    assert_eq!(body["build"]["version"], env!("CARGO_PKG_VERSION"));
    if let Some(commit) = body["build"]["commit"].as_str() {
        assert!((7..=64).contains(&commit.len()));
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(body["build"]["commit_source"].is_string());
    } else {
        assert!(body["build"]["commit_source"].is_null());
    }
    assert!(body["runtime"]["executable_path"].is_string());
    assert_eq!(body["installation"]["binary_on_disk"]["probe_status"], "ok");
    assert_eq!(
        body["installation"]["binary_on_disk"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        body["installation"]["binary_on_disk"]["newer_than_running"],
        false
    );
    assert_eq!(body["ipc"]["configured"], true);
    assert_eq!(
        body["ipc"]["endpoint"],
        "ipc://diagnostic-test.sock [query/fragment redacted]"
    );
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(!serialized.contains("token=secret"), "{body:#?}");
    assert!(!serialized.contains("#fragment"), "{body:#?}");
    assert!(!body["restart_guidance"].as_array().unwrap().is_empty());
}

#[test]
fn file_based_tool_roundtrip_in_temp_project() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proto_demo");
    let mut p = McpProcess::spawn();

    let created = p.call_tool(
        "create_project",
        json!({"name": "proto_demo", "path": proj.to_string_lossy()}),
    );
    assert_ne!(
        created["isError"],
        json!(true),
        "create_project failed: {created}"
    );
    assert!(proj.join("proto_demo.kicad_sch").exists());

    let info = p.call_tool(
        "get_project_info",
        json!({"path": proj.join("proto_demo.kicad_pro").to_string_lossy()}),
    );
    assert_ne!(
        info["isError"],
        json!(true),
        "get_project_info failed: {info}"
    );
}

#[test]
fn structured_errors_guide_recovery() {
    let mut p = McpProcess::spawn();

    // Known tool in an unloaded toolset → toolset_not_loaded naming the owner.
    let r = p.call_tool("route_trace", json!({}));
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "toolset_not_loaded");
    assert_eq!(body["error"]["toolset"], "pcb_routing");

    // Unknown tool → unknown_tool.
    let r = p.call_tool("frobnicate_board", json!({}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "unknown_tool");

    // Missing required argument → invalid_argument naming the field.
    let r = p.call_tool("create_project", json!({"path": "/tmp/x"}));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "name");
}

#[test]
fn unknown_method_is_json_rpc_error_not_crash() {
    let mut p = McpProcess::spawn();
    let resp = p.request("tools/definitely_not_a_method", json!({}));
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error: {resp}"
    );
    // Server must still be alive afterwards.
    let ping = p.request("ping", json!({}));
    assert!(ping.get("result").is_some());
}

/// Regression test for issue #19. After `load_toolset`, the server must emit
/// `notifications/tools/list_changed` **over stdio** — not only over HTTP/SSE.
/// Without it, stdio clients (Claude Code) never re-fetch `tools/list`, so
/// every tool added by `load_toolset` stays uncallable for the session.
#[test]
fn load_toolset_emits_list_changed_over_stdio() {
    let mut p = McpProcess::spawn();
    let lines = p.call_tool_then_fence("load_toolset", json!({"name": "sch_components"}));
    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after load_toolset (issue #19); saw: {lines:#?}"
    );
}

/// The same guarantee for `unload_toolset` — removing tools must also tell the
/// client to refresh its tool list.
#[test]
fn unload_toolset_emits_list_changed_over_stdio() {
    let mut p = McpProcess::spawn();
    let _ = p.call_tool_then_fence("load_toolset", json!({"name": "sch_components"}));
    let lines = p.call_tool_then_fence("unload_toolset", json!({"name": "sch_components"}));
    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after unload_toolset; saw: {lines:#?}"
    );
}

/// `load_toolset` accepts an array of names in one call: all listed toolsets
/// load, tools_added sums across them, and only one list_changed notification
/// fires for the whole batch.
#[test]
fn load_toolset_batch_form_loads_all_and_notifies_once() {
    let mut p = McpProcess::spawn();
    let lines = p.call_tool_then_fence(
        "load_toolset",
        json!({"name": ["sch_components", "sch_wiring"]}),
    );
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    let body = McpProcess::tool_body(&r);
    // sch_components (18) + sch_wiring (20).
    assert_eq!(body["tools_added"].as_u64(), Some(38));
    // tools items are {name, description} objects, matching the legacy
    // single-name result shape -- not bare name strings.
    let tools = body["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
    for t in tools {
        assert!(t.get("name").and_then(Value::as_str).is_some(), "{t:#?}");
        assert!(
            t.get("description").and_then(Value::as_str).is_some(),
            "{t:#?}"
        );
    }

    let notification_count = lines
        .iter()
        .filter(|v| {
            v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
                && v.get("id").is_none()
        })
        .count();
    assert_eq!(
        notification_count, 1,
        "expected exactly one list_changed notification for the batch; saw: {lines:#?}"
    );

    // Mixed valid/invalid names: partial failure is not isError, but the
    // errors array names the unknown toolset and loaded lists only the real one.
    let lines = p.call_tool_then_fence(
        "load_toolset",
        json!({"name": ["templates", "bogus_toolset"]}),
    );
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    assert_ne!(r["isError"].as_bool(), Some(true), "{r:#?}");
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["loaded"], json!(["templates"]));
    let errors = body["errors"].as_array().expect("errors array");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].as_str().unwrap().contains("list_toolboxes"),
        "{errors:#?}"
    );
}

/// All names in one `load_toolset` call unknown -> a typed `invalid_argument`
/// error (not a JSON body with a hand-set `isError`), so the observer keeps a
/// real `error_kind` column instead of degrading to `handler_error`.
#[test]
fn load_toolset_batch_total_failure_is_typed_error() {
    let mut p = McpProcess::spawn();
    let r = p.call_tool("load_toolset", json!({"name": ["bogus_one", "bogus_two"]}));
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "name");
    assert!(
        body["message"].as_str().unwrap().contains("list_toolboxes"),
        "{body:#?}"
    );
}

/// With `auto_load_toolsets = true` in `konnect.toml` (picked up from the
/// server process's cwd), calling a tool from an unloaded toolset auto-loads
/// it and executes in the same call instead of returning `toolset_not_loaded`.
/// Default-off behavior (no config file) is covered by
/// `structured_errors_guide_recovery`.
#[test]
fn auto_load_toolsets_config_loads_and_executes_on_miss() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("konnect.toml"),
        "auto_load_toolsets = true\n",
    )
    .unwrap();
    let mut p = McpProcess::spawn_in_dir(Some(tmp.path()));

    // route_trace is in pcb_routing, not loaded at startup. With auto-load on,
    // the toolset loads, a list_changed notification fires, and the call gets
    // as far as argument validation instead of failing with
    // toolset_not_loaded — which is what this test is about.
    //
    // The field named is `board`, the first entry in route_trace's own
    // `required` list. This used to be `net_name`, whichever argument the
    // handler happened to read first; since #218 the dispatch checks
    // `required` in schema order before the handler runs, which is the order
    // the client was shown.
    let lines = p.call_tool_then_fence("route_trace", json!({}));
    let r = lines
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("expected a tools/call result")["result"]
        .clone();
    assert_eq!(r["isError"], json!(true));
    let body = McpProcess::tool_body(&r);
    assert_eq!(body["error"]["kind"], "invalid_argument");
    assert_eq!(body["error"]["field"], "board");

    let saw_notification = lines.iter().any(|v| {
        v.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
            && v.get("id").is_none()
    });
    assert!(
        saw_notification,
        "expected notifications/tools/list_changed after auto-load; saw: {lines:#?}"
    );
}

// ─── Config provenance over the protocol (#419) ───────────────────────────────
//
// These spawn the real binary, so they prove what a client actually receives —
// not what the resolver returns in isolation. Paths are compared after
// `canonicalize` because macOS resolves a temp dir under /var to /private/var,
// and the server reports the resolved form.

fn canonical(path: &std::path::Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn configuration_block(p: &mut McpProcess) -> Value {
    let result = p.call_tool("get_installation_info", json!({}));
    assert_ne!(result["isError"], json!(true), "{result:#?}");
    McpProcess::tool_body(&result)["configuration"].clone()
}

#[test]
fn installation_info_names_the_config_file_that_configured_startup() {
    let tmp = tempfile::tempdir().unwrap();
    let selected = tmp.path().join("konnect.toml");
    std::fs::write(&selected, "log_level = \"debug\"\n").unwrap();

    let mut p = McpProcess::spawn_configured(Some(tmp.path()), true);
    let configuration = configuration_block(&mut p);

    assert_eq!(configuration["source"], "search_path");
    assert_eq!(configuration["selected_path"], canonical(&selected));
    assert_eq!(configuration["search_policy"], "first_existing_no_merge");
    assert_eq!(configuration["skipped_existing_paths"], json!([]));
}

#[test]
fn installation_info_reports_a_shadowed_later_candidate() {
    // konnect.toml precedes settings.json, so the second exists but is not read.
    // Naming it is the whole point: "my settings.json is ignored" was
    // indistinguishable from "my settings.json is malformed" (#419).
    let tmp = tempfile::tempdir().unwrap();
    let selected = tmp.path().join("konnect.toml");
    let shadowed = tmp.path().join("settings.json");
    std::fs::write(&selected, "log_level = \"debug\"\n").unwrap();
    std::fs::write(&shadowed, "{\"log_level\": \"trace\"}").unwrap();

    let mut p = McpProcess::spawn_configured(Some(tmp.path()), true);
    let configuration = configuration_block(&mut p);

    assert_eq!(configuration["source"], "search_path");
    assert_eq!(configuration["selected_path"], canonical(&selected));
    assert_eq!(
        configuration["skipped_existing_paths"],
        json!([canonical(&shadowed)]),
        "the shadowed file must be named, not merged and not hidden"
    );
}

#[test]
fn installation_info_reports_startup_state_not_a_fresh_search() {
    // Start with only settings.json, then create the higher-priority
    // konnect.toml while the server runs. The answer must not change: the
    // question is what configured this process, not what would configure a new
    // one.
    let tmp = tempfile::tempdir().unwrap();
    let selected = tmp.path().join("settings.json");
    std::fs::write(&selected, "{\"log_level\": \"debug\"}").unwrap();

    let mut p = McpProcess::spawn_configured(Some(tmp.path()), true);
    assert_eq!(
        configuration_block(&mut p)["selected_path"],
        canonical(&selected)
    );

    std::fs::write(tmp.path().join("konnect.toml"), "log_level = \"trace\"\n").unwrap();

    let after = configuration_block(&mut p);
    assert_eq!(
        after["selected_path"],
        canonical(&selected),
        "a file created after launch must not be reported as having configured this process"
    );
    assert_eq!(after["skipped_existing_paths"], json!([]));
}

#[test]
fn installation_info_reports_defaults_when_no_config_file_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let mut p = McpProcess::spawn_configured(Some(tmp.path()), true);

    let configuration = configuration_block(&mut p);

    assert_eq!(configuration["source"], "defaults");
    assert_eq!(configuration["selected_path"], Value::Null);
    assert_eq!(configuration["skipped_existing_paths"], json!([]));
}

#[test]
fn env_fallback_applies_without_disturbing_reported_provenance() {
    // Acceptance row: a selected file with a blank ipc_address plus
    // KICAD_API_SOCKET in the environment. The env fallback must still be
    // applied after file resolution (#39), and provenance must still name the
    // file that was selected -- the env var configures a value, not a source.
    let tmp = tempfile::tempdir().unwrap();
    let selected = tmp.path().join("konnect.toml");
    std::fs::write(&selected, "ipc_address = \"\"\n").unwrap();

    let mut p = McpProcess::spawn_with_env(
        Some(tmp.path()),
        true,
        &[("KICAD_API_SOCKET", "ipc://konnect-419-env.sock")],
    );

    let result = p.call_tool("get_installation_info", json!({}));
    assert_ne!(result["isError"], json!(true), "{result:#?}");
    let body = McpProcess::tool_body(&result);

    assert_eq!(body["configuration"]["source"], "search_path");
    assert_eq!(body["configuration"]["selected_path"], canonical(&selected));
    assert_eq!(
        body["ipc"]["configured"],
        json!(true),
        "the blank ipc_address should have been filled from the environment"
    );
}

/// Run the server to completion with the given args, returning (exit ok, stderr).
/// Used for the cases where startup must FAIL: the MCP harness above expects a
/// handshake, and there deliberately is not one.
fn run_expecting_startup_failure(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_konnect"))
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("APPDATA", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run konnect binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_malformed_selected_file_stops_startup_instead_of_falling_through() {
    // konnect.toml is selected and is broken; settings.json is valid and later.
    // Falling through would silently run on a configuration the user never
    // pointed at, while their real file went unreported — the failure #419 is
    // about, in its worst form.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("konnect.toml"), "not = valid toml [[[").unwrap();
    std::fs::write(
        tmp.path().join("settings.json"),
        "{\"log_level\":\"trace\"}",
    )
    .unwrap();

    let (ok, stderr) = run_expecting_startup_failure(tmp.path(), &[]);

    assert!(!ok, "a malformed selected file must not start the server");
    assert!(
        !stderr.contains("configuration: search_path"),
        "startup must not report a selection it never completed: {stderr}"
    );
}

#[test]
fn a_malformed_explicit_config_stops_startup_instead_of_falling_back() {
    // --config bypasses discovery, so a valid settings.json sitting next to it
    // must not rescue a broken explicit file either.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("bad.toml"), "not = valid toml [[[").unwrap();
    std::fs::write(
        tmp.path().join("settings.json"),
        "{\"log_level\":\"trace\"}",
    )
    .unwrap();

    let (ok, stderr) = run_expecting_startup_failure(tmp.path(), &["--config", "bad.toml"]);

    assert!(
        !ok,
        "a malformed explicit --config must not start the server"
    );
    assert!(
        !stderr.contains("configuration:"),
        "startup must not report any configuration selection: {stderr}"
    );
}

/// `ipc_failure` (#532) must reach a client over the real stdio protocol path,
/// not only the in-process handler. The endpoint is a socket nothing listens
/// on; the working directory and home are isolated so `KICAD_API_SOCKET` is
/// the address that resolves.
#[test]
fn ipc_failure_kind_and_message_reach_the_client_over_stdio() {
    let tmp = tempfile::tempdir().unwrap();
    let address = format!("ipc://{}", tmp.path().join("no-kicad-here.sock").display());
    let mut p = McpProcess::spawn_with_env(
        Some(tmp.path()),
        true,
        &[("KICAD_API_SOCKET", address.as_str())],
    );
    McpProcess::tool_body(
        &p.call_tool("load_toolset", json!({"name": ["project", "verification"]})),
    );

    for (tool, args) in [
        ("check_kicad_ui", json!({"timeout_seconds": 60})),
        ("open_project", json!({})),
    ] {
        let result = p.call_tool(tool, args);
        assert_ne!(result["isError"], json!(true), "{tool}: {result}");
        let body = McpProcess::tool_body(&result);
        assert_eq!(body["ipc_failure"]["kind"], "no_listener", "{tool}: {body}");
        assert!(
            body["ipc_failure"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Nothing is listening there")),
            "{tool}: {body}"
        );
    }
}

/// Where `user_config_path()` puts the preferences file when `HOME` /
/// `APPDATA` point at `home`.
fn user_config_under(home: &std::path::Path) -> std::path::PathBuf {
    let dir = if cfg!(target_os = "windows") {
        home.join("konnect")
    } else if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("konnect")
    } else {
        home.join(".konnect")
    };
    dir.join("config.json")
}

/// #580 over real stdio: a user preferences file that exists and cannot be
/// parsed is refused by the load, and the save that used to replace it with
/// the defaults refuses too and leaves it byte-identical.
#[test]
fn a_malformed_user_configuration_is_refused_and_never_overwritten_over_stdio() {
    let tmp = tempfile::tempdir().unwrap();
    let config = user_config_under(tmp.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original =
        r#"{"kicad_cli": "D:/tools/kicad-cli.exe", "sourcing": {"avl": ["Vishay", "Murata"]},"#;
    std::fs::write(&config, original).unwrap();

    let mut p = McpProcess::spawn_with_env(Some(tmp.path()), true, &[]);
    for (tool, args) in [
        ("load_user_config", json!({})),
        (
            "save_user_config",
            json!({"key_path": "ui.theme", "value": "dark"}),
        ),
        ("get_effective_config", json!({})),
    ] {
        let result = p.call_tool(tool, args);
        assert_eq!(result["isError"], json!(true), "{tool}: {result}");
        let body = McpProcess::tool_body(&result);
        assert_eq!(
            body["error"]["kind"], "invalid_configuration",
            "{tool}: {body}"
        );
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            original,
            "{tool} left the preferences file exactly as it was"
        );
    }
}

/// The other half: with no preferences file the load says it answered with
/// the defaults, leaves them on disk, and the next load reads that file.
#[test]
fn an_absent_user_configuration_reports_defaults_over_stdio() {
    let tmp = tempfile::tempdir().unwrap();
    let config = user_config_under(tmp.path());
    let mut p = McpProcess::spawn_with_env(Some(tmp.path()), true, &[]);

    let first = McpProcess::tool_body(&p.call_tool("load_user_config", json!({})));
    assert_eq!(first["source"], "defaults", "{first}");
    assert_eq!(first["persisted"], true, "{first}");
    assert!(config.is_file(), "the defaults were written to {config:?}");

    let second = McpProcess::tool_body(&p.call_tool("load_user_config", json!({})));
    assert_eq!(second["source"], "file", "{second}");
}

/// The served binary compares installed guidance with its own bundle once per
/// installed version and tells both the model and `get_installation_info`
/// (#728). It writes only its record under `.konnect`, and a second start with
/// the same install neither rescans nor repeats the notice.
/// Unix only: `dirs::home_dir` ignores `HOME` on Windows, so the child would
/// read the real profile there.
#[cfg(unix)]
#[test]
fn a_legacy_guidance_marker_is_reported_on_the_first_start_only() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".konnect")).unwrap();
    std::fs::write(tmp.path().join(".konnect/.installed"), "0.2.2").unwrap();
    let mut p = McpProcess::spawn_configured(Some(tmp.path()), true);
    let instructions = p.initialized["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.starts_with("Konnect Claude guidance (installed by v0.2.2)"),
        "{instructions}"
    );
    assert!(instructions.contains("`konnect init`"), "{instructions}");
    assert_eq!(instructions.lines().count(), 1, "Codex was never installed");

    let result = p.call_tool("get_installation_info", json!({}));
    let body = McpProcess::tool_body(&result);
    let guidance = &body["guidance"];
    assert_eq!(guidance["probe_status"], "ok");
    assert_eq!(guidance["claude"]["state"], "out_of_sync");
    assert_eq!(guidance["claude"]["checked"], "now");
    assert_eq!(guidance["claude"]["marker"]["version"], "0.2.2");
    assert_eq!(guidance["codex"]["state"], "not_installed");
    assert!(guidance["claude"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|file| file["status"] == "missing"));
    drop(p);

    let mut p = McpProcess::spawn_configured(Some(tmp.path()), true);
    let init = &p.initialized["result"];
    assert!(init.get("instructions").is_none(), "{init}");
    let result = p.call_tool("get_installation_info", json!({}));
    let guidance = &McpProcess::tool_body(&result)["guidance"];
    assert_eq!(guidance["claude"]["state"], "out_of_sync");
    assert_eq!(guidance["claude"]["checked"], "earlier");
    assert!(guidance["claude"].get("files").is_none(), "{guidance}");
    drop(p);

    for dir in [".claude", ".agents"] {
        assert!(!tmp.path().join(dir).exists(), "detection wrote {dir}");
    }
    let mut data = std::fs::read_dir(tmp.path().join(".konnect"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        // The call log and runtime directory every server start keeps.
        .filter(|name| name != "logs" && name != "run")
        .collect::<Vec<_>>();
    data.sort();
    assert_eq!(
        data,
        [
            ".guidance-checked-claude",
            ".guidance-checked-codex",
            ".installed"
        ]
    );
}

/// #787 over real stdio: with nothing to find, `check_freerouting` reports
/// every location it searched, in the documented order, from the server's own
/// environment. The environment is redirected so a Freerouting installed on
/// the machine running the test cannot be found.
#[test]
fn check_freerouting_reports_every_location_it_searched_over_stdio() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let third_party: Vec<String> = ["3rd-10", "3rd-9", "3rd-8"]
        .iter()
        .map(|name| {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir.display().to_string()
        })
        .collect();
    let (bin_s, home_s) = (bin.display().to_string(), tmp.path().display().to_string());
    let mut p = McpProcess::spawn_with_env(
        Some(tmp.path()),
        true,
        &[
            ("PATH", bin_s.as_str()),
            ("USERPROFILE", home_s.as_str()),
            ("KICAD10_3RD_PARTY", third_party[0].as_str()),
            ("KICAD9_3RD_PARTY", third_party[1].as_str()),
            ("KICAD8_3RD_PARTY", third_party[2].as_str()),
        ],
    );
    McpProcess::tool_body(&p.call_tool("load_toolset", json!({"name": "integration"})));

    let result = p.call_tool("check_freerouting", json!({}));
    assert_ne!(result["isError"], json!(true), "{result}");
    let body = McpProcess::tool_body(&result);
    assert_eq!(body["engine_found"], false, "{body}");
    assert_eq!(body["available"], false, "{body}");

    let searched = body["searched_locations"]
        .as_array()
        .expect("searched_locations is reported");
    let kinds: Vec<&str> = searched
        .iter()
        .map(|root| root["kind"].as_str().unwrap())
        .collect();
    // `HOME` and `USERPROFILE` name the same directory here, so on Windows the
    // `Documents` plugin folders are reached twice and searched once.
    let plugin_folders = 6;
    let mut expected = vec!["kicad_3rd_party_variable"; 3];
    expected.extend(vec!["kicad_plugin_folder"; plugin_folders]);
    expected.extend(["working_directory", "system_directory", "system_directory"]);
    expected.push("path_directory");
    assert_eq!(kinds, expected, "{body}");

    for (root, dir) in searched[..3].iter().zip(&third_party) {
        assert_eq!(root["path"], json!(dir), "{root}");
        assert_eq!(root["exists"], true, "{root}");
    }
    let working = std::path::PathBuf::from(searched[3 + plugin_folders]["path"].as_str().unwrap());
    assert_eq!(working.file_name().unwrap(), "freerouting.jar");
    assert_eq!(
        working.parent().unwrap().canonicalize().unwrap(),
        tmp.path().canonicalize().unwrap()
    );
    assert_eq!(searched[3 + plugin_folders]["exists"], false);
    let last = searched.last().unwrap();
    assert_eq!(last["path"], json!(bin_s), "{last}");
    assert_eq!(last["max_depth"], 1, "{last}");
}
