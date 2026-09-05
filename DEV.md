# Developer Guide — Konnect

Internal reference for developing and maintaining the Rust port.

New to the codebase? Start with the map in
[docs/DEVELOPER_OVERVIEW.md](docs/DEVELOPER_OVERVIEW.md), then return here for
the detailed implementation reference and current statistics.

Repository-wide naming, public API, branch, and pull-request rules live in
[docs/NAMING_CONVENTIONS.md](docs/NAMING_CONVENTIONS.md).

## Quick Start

```bash
# protoc is required for protobuf code generation. If PROTOC is unset, the
# build falls back to `protoc` on PATH (see Build Requirements below).
set PROTOC=C:\path\to\protoc.exe   # or install via `choco install protoc`

cargo check                          # verify everything compiles (~15s)
cargo test --workspace --lib --tests # all tests
cargo build --release -p konnect # build the MCP server binary

# Build the schematic viewer (separate crate)
cd crates/schematic-viewer
cargo build --release
```

Schematic-viewer build notes (Windows):

- If `cargo` is not recognized in a fresh shell, add it to the session PATH first:
  `set PATH=%PATH%;%USERPROFILE%\.cargo\bin`
- Close any running viewer window before rebuilding — Windows locks a running
  `.exe`, so the link step fails while the app is open.

### Nix

A `flake.nix` provides a reproducible build and dev shell on Linux
(`x86_64`/`aarch64`), contributed in #198:

```bash
nix build .#konnect   # build the server binary
nix run  .#konnect    # build and run it
nix develop           # shell with the pinned toolchain, protoc, kicad-small
```

The flake pins its own toolchain from `rust-toolchain.toml` and reads the
version from the workspace `Cargo.toml`, so those stay in step automatically.
What it does *not* track automatically is the build itself: `cargoBuildFlags`
names `-p konnect --bin konnect`, and `preCheck` exports `KONNECT_STATE_DIR`
for the tests that need a writable state dir. **Renaming the binary, adding a
workspace member the server depends on, or introducing a test that needs
another environment variable will break the Nix build without breaking any
other job** — the `Nix flake` CI job exists to catch exactly that, so treat a
failure there as a real break rather than a Nix-user problem.

## Architecture

```
Konnect/
├── crates/
│   ├── konnect/              # Main binary + cdylib entry points
│   │   └── src/
│   │       ├── main.rs              # CLI: --config, subcommands
│   │       ├── lib.rs               # cdylib re-exports ffi
│   │       ├── ffi.rs               # C ABI: kicad_plugin_init/version/shutdown
│   │       ├── config.rs            # TOML + JSON config, socket path auto-detection
│   │       └── transport/
│   │           ├── stdio.rs         # Line-by-line JSON-RPC over stdin/stdout (default)
│   │           └── http.rs          # Streamable HTTP: POST + GET (SSE) on /mcp (transport = "http" / "both")
│   │
│   ├── konnect-core/          # All tool logic (20 toolsets)
│   │   └── src/
│   │       ├── mcp/
│   │       │   ├── protocol.rs      # MCP JSON-RPC 2.0 types
│   │       │   ├── handler.rs       # Dispatch: initialize, tools/list (all tools static), tools/call
│   │       │   └── server.rs        # Session state machine
│   │       ├── router/
│   │       │   ├── mod.rs           # ToolRouter: load/unload toolsets
│   │       │   ├── registry.rs      # Static toolset metadata + tools_for() dispatcher
│   │       │   └── meta_tools.rs    # 7 always-visible meta-tools
│   │       └── tools/
│   │           ├── mod.rs            # ToolDef, ToolContext, tool! macro, helpers, kicad_config_dir()
│   │           ├── cli.rs            # kicad-cli v10 subprocess wrapper (verified against actual binary)
│   │           ├── svg_import.rs     # SVG parsing + Bezier flattening for import_svg_logo (usvg-backed)
│   │           ├── project.rs        # 7 tools (incl. open_schematic_viewer)
│   │           ├── sch_components.rs # 20 tools (component placement with lib_symbols embedding)
│   │           ├── sch_wiring.rs     # 20 tools (incl. connect_pins, power symbol embedding)
│   │           ├── sch_analysis.rs   # 15 tools (union-find net graph, connectivity)
│   │           ├── sch_batch.rs      # 12 tools (single-read/single-write atomic operations)
│   │           ├── sch_export.rs     # 7 tools (SVG/PDF/netlist/ERC/PCB sync)
│   │           ├── sch_bus.rs        # 4 tools (buses, bus entries, pin fan-out)
│   │           ├── pcb_sync.rs       # update_pcb_from_schematic: pure planner + one-commit IPC apply
│   │           ├── sch_hierarchy.rs  # 12 tools (typed Sheet model, sheet CRUD + hierarchy/page queries + pin lifecycle)
│   │           ├── pcb_board.rs      # 11 tools (S-expr file editing, IPC fallback, SVG logo import)
│   │           ├── pcb_components.rs # 19 tools (IPC real-time + safe headless single-placement fallback)
│   │           ├── pcb_footprint_update.rs # library refresh planner + one-commit IPC apply
│   │           ├── pcb_routing.rs    # 15 tools (traces, vias, nets, netclasses, SES import)
│   │           ├── pcb_export.rs     # 14 tools (Gerber, PDF, 3D, Specctra DSN, DRC, DXF/GenCAD/IPC-2581/ODB++)
│   │           ├── library.rs        # 17 tools (symbol/footprint library management)
│   │           ├── footprint_graphics.rs # footprint primitive validation, inspection, and atomic edits
│   │           ├── footprint_metadata.rs # footprint description, tags, and attribute edits
│   │           ├── footprint_models.rs # footprint 3D model validation and atomic edits
│   │           ├── integration.rs    # 9 tools (JLCPCB SQLite, Freerouting MCP, datasheets)
│   │           ├── verification.rs   # 10 tools (DRC, design rules, KiCAD UI)
│   │           ├── config.rs         # 7 tools (user/project config, design rules)
│   │           ├── design_review.rs  # 6 tools (decoupling/connection/power/DFM audits)
│   │           ├── templates.rs      # 4 tools (6 built-in reference circuit templates)
│   │           └── manufacturing.rs  # 3 tools (export package, validate, cost estimate)
│   │
│   ├── konnect-sexp/                  # S-expression engine (no KiCAD dependency)
│   │   └── src/
│   │       ├── parser.rs             # nom-based parser (handles empty strings)
│   │       ├── writer.rs             # SexpEdit + apply_edits + write_atomic
│   │       ├── schematic.rs          # SymbolInstance, LibPin, extract_*, pin_endpoint
│   │       ├── geometry.rs           # PinTransform, transform_pin (CANONICAL pin math)
│   │       ├── net.rs                # NetRef — KiCad-10 vs legacy net node forms
│   │       ├── layers.rs             # Canonical layer names, copper stack queries
│   │       ├── command.rs            # Reversible edit commands
│   │       ├── transaction.rs        # Write-ahead journal for multi-file writes
│   │       └── error.rs              # SexpError (incl. Conflict)
│   │
│   ├── konnect-schematic-editor/      # Typed schematic model (parse → mutate → write)
│   │   └── src/
│   │       ├── schematic/            # Schematic, symbol, wire, label, sheet, misc
│   │       ├── sexp/                 # parser + writer (indent-preserving, see #210)
│   │       ├── library.rs            # Library symbol lookup
│   │       └── types.rs              # Shared value types (fmt_f64, …)
│   │
│   ├── konnect-ipc/                   # KiCAD 10 IPC API client
│   │   ├── proto/                    # Protobuf definitions (copied from KiCAD v10 source)
│   │   ├── build.rs                  # prost-build protobuf code generation
│   │   └── src/
│   │       ├── gen.rs                # Generated protobuf Rust types
│   │       ├── client.rs             # NNG req/rep client, all methods implemented
│   │       ├── builders.rs           # Protobuf message construction helpers (mm→nm conversion)
│   │       ├── transform.rs          # Rigid-body child transform — KiCAD 10 stores footprint
│   │       │                         # children in ABSOLUTE board coords (#23)
│   │       └── types.rs              # Public types (IpcFootprint, IpcTrack, etc.)
│   │
│   └── schematic-viewer/            # Tauri desktop app (separate from workspace)
│       ├── tauri.conf.json
│       ├── capabilities/default.json # Tauri 2 ACL grant (core:default) — without it event.listen() is silently denied
│       ├── src/main.rs               # Multi-sheet watcher + snapshot-isolated incremental kicad-cli SVG rendering + Tauri commands, 20 unit tests
│       └── frontend/index.html       # Pan/zoom SVG viewer, sheet selector, auto-refresh
│
├── plugin/                           # Python thin launcher (runs inside KiCAD)
│   ├── __init__.py                   # pcbnew.ActionPlugin — settings dialog (PCB Editor only)
│   ├── settings_dialog.py            # wxPython settings UI (paths, server control)
│   ├── native_bridge.py              # authenticated KiCad 10 native Specctra export bridge
│   └── plugin.json                   # KiCAD 10 IPC plugin manifest
│
├── packaging/
│   ├── build-pcm.ps1                 # Build the PCM zip (Windows)
│   ├── build-pcm.sh                  # Build the PCM zip (macOS/Linux)
│   ├── metadata.json                 # KiCAD PCM package manifest
│   ├── validate-pcm.py               # Validate metadata.json against the PCM schema
│   ├── schema/                       # PCM packages.v1 JSON schema
│   └── resources/                    # PCM package resources (icon.png)
│
└── .github/workflows/
    ├── ci.yml                        # 7 jobs: check+test (3-OS matrix), clippy and
    │                                 # fmt (ubuntu only), schematic viewer, Python
    │                                 # plugin, Nix flake, PCM packaging validation.
    │                                 # The last four cover code `cargo --workspace`
    │                                 # never sees. 9 check runs per PR — the matrix
    │                                 # counts as three.
    ├── e2e-kicad.yml                 # Real KiCAD 10.0.5: end-to-end suite, a
    │                                 # conformance pass over KiCAD's demo corpus, and
    │                                 # PCM assembly + schema validation. Weekly cron,
    │                                 # manual dispatch, and `v*` tag push — not
    │                                 # per-PR, and it does not gate release.yml
    └── release.yml                   # 3 jobs: build (4 targets), pcm-package (3
                                      # platforms, macOS universal via lipo), release
```

## KiCAD 10 Integration

### IPC API (PCB Editor — real-time)
- Transport: **NNG** (nanomsg-next-gen) over IPC sockets (Windows named pipes)
- Protocol: **Protocol Buffers** (protobuf3) with ApiRequest/ApiResponse envelope
- Socket path: from `KICAD_API_SOCKET` environment variable (set by KiCAD when launching plugins)
- Scope: **PCB editor only** — full CRUD on all board items, layer management, design rules
- Schematic editor IPC: export-only (SVG, PDF, BOM, netlist) — NO item CRUD

### S-Expression File Editing (Schematic — offline)
- Direct read/write of `.kicad_sch` files
- Symbol definitions auto-embedded from KiCAD 10's `.kicad_symdir` format
- Power symbols (VCC, GND) embedded from `power.kicad_symdir`
- Existing-file edits use revision-checked atomic replacement: read the exact
  source, acquire a cooperative lock, reject any intervening KiCad or Konnect
  change, write a unique sibling scratch file, fsync, and rename.
- Cooperative lock files live under `KONNECT_STATE_DIR/locks` when that
  absolute override is set, otherwise under the platform local-data directory
  (`konnect/locks`). Reads never create files in the KiCad project.
- Schematic writes also refuse while KiCad's sibling `~<name>.kicad_sch.lck`
  exists. KiCad records only a username and hostname, so Konnect cannot prove
  that a same-host or remote lock is stale; valid, foreign, empty, and malformed
  locks all fail closed. The check runs before a transaction journal is created
  and again at the final target-write boundary.
- Multi-file schematic changes use project-local
  `.konnect-transaction-*.json` write-ahead journals. These journals contain
  complete before/after images and must be treated as sensitive project data.

`konnect_schematic_editor::Schematic` deliberately distinguishes creation from
replacement:

- `save(new_path)` is create-only and refuses to replace an existing path.
- `save(loaded_path)` and `overwrite()` replace only when the file still
  exactly matches the source loaded into the model. KiCad autosave therefore
  produces a conflict that callers must resolve by reloading and reapplying.
- Callers that intentionally replace an existing file must use the explicit
  revision-aware writer/command APIs; they must not delete the destination or
  weaken `save()` into an unconditional overwrite.

For journal diagnosis and recovery, use `konnect transaction status`,
`konnect transaction recover`, and the explicit force-gated `konnect
transaction abandon` escape hatch documented in
[Troubleshooting](docs/TROUBLESHOOTING.md#transaction-recovery-is-blocked-by-divergent-content).

### kicad-cli v10 (Subprocess)
- Verified commands: `sch erc`, `sch export svg/pdf/bom/netlist`, `pcb drc`, `pcb export gerbers/drill/pdf/svg/step/vrml/pos/ipcd356`, `pcb render`
- Removed in v10: `sch annotate` (reimplemented in Rust), `pcb sync`, `pcb export/import specctra`
- Version format: `20250610`

### Plugin Installation
- **PCM zip** is the correct install method
- KiCad installs to: `C:\Users\<YOU>\Documents\KiCad\10.0\3rdparty\plugins\com_github_mixelpixx_konnect\`
- Both `__init__.py` (SWIG ActionPlugin for PCB editor settings dialog) and `plugin.json` (IPC exec plugin) are included
- `native_bridge.py` is a KiCad-10-only, opt-in compatibility bridge. It exposes
  only authenticated status and native Specctra export over an ephemeral
  loopback port. The caller cannot choose an output path; the plugin owns and
  removes the temporary artifact. Do not grow it into a general Python RPC
  surface or use it as the KiCad 11 architecture. The Rust exporter remains
  the default; callers must explicitly choose `prefer` or `require` to use this
  compatibility path.

## Structured Errors

Tool-call failures are typed via the `ToolErrorKind` enum in `crates/konnect-core/src/mcp/error.rs`. MCP's `CallToolResult` spec has no top-level `data` field, so structured errors ride inside the text content as JSON:

```json
{
  "message": "Tool 'place_component' is in toolset 'pcb_components' — call load_toolset('pcb_components') first, then retry.",
  "error": {
    "kind": "toolset_not_loaded",
    "toolset": "pcb_components",
    "tool": "place_component"
  }
}
```

`is_error: true` on the result; plain clients show the `message` field, structured clients match on `kind`. The observer's `error_kind` column is populated via `extract_error_kind()` so JSONL logs use the same vocabulary regardless of where the error originated.

### Current kinds

| `kind` | When |
|--------|------|
| `toolset_not_loaded` | Tool exists but its toolset isn't loaded yet |
| `unknown_tool` | Tool name doesn't exist in any toolset |
| `invalid_argument` | Required argument missing/malformed |
| `file_not_found` | Referenced file or project-discovery directory does not exist or cannot be read |
| `conflict` | The file changed, a write would replace existing paths, or schematic project ownership cannot be proven uniquely — carries the affected paths; ownership conflicts include the schematic directory and all candidate roots |
| `ambiguous_target` | More than one observed UUID, reference/unit, field, or instance identity matches the requested target, or component instance records conflict across projects, units, or hierarchy paths — carries `target` and the stable `candidates`; the caller must choose rather than retrying blindly |
| `stale_target` | Saved symbol instance metadata disagrees with the proven hierarchy, or component readback lacks the expected document, UUID/reference, unit, library, hierarchy, position, rotation, or property value — carries `target` and `reason`; preflight refuses before writing, while a readback failure may follow a committed write |
| `handler_error` | Catch-all for unmigrated `anyhow::Error` returns |

### Producing structured errors in a handler

```rust
if !path.exists() {
    return Ok(CallToolResult::error_kind(
        ToolErrorKind::FileNotFound { path: path.display().to_string() },
        format!("Project file not found: {}", path.display()),
    ));
}
```

Adding a new kind: edit `mcp/error.rs`, add the variant, add the match arm in `short_code()`, use it from the handler. The `short_code_matches_serialized_kind_field` test will fail loudly if they drift.

The dispatch-level errors (not-loaded/unknown/handler-panic) are fully structured, and so is **every missing-argument error**. Three mechanisms produce them, in the order a call meets them:

1. **The schema gate.** Before a handler runs, `handler.rs::dispatch_tool` calls `first_missing_required(&tool_def.input_schema, args)` and refuses with `invalid_argument` naming the first absent entry of the tool's own `required` list, in schema order. Presence only — a value of the wrong *type* passes through to the handler. An explicit `null` counts as absent, because every `as_str()`/`as_array()` read treats it that way.

2. **The `require_*` helpers**, in `tools/mod.rs`: `require_str`, `require_f64`, `require_array`, `require_u64`. Each returns a `CallToolResult` carrying `InvalidArgument { field, reason }`, so a handler propagates it with `Err(e) => return Ok(e)`. These name the field for a *wrong type*, which the schema gate cannot see, and they are what makes a handler correct when called directly — as most tests do.

3. **`get_path`**, which returns `anyhow::Result<PathBuf>` so its 171 call sites can use `?`. Changing that signature was not worth it, so it attaches a `MissingArgument` marker to the error and `dispatch_tool` downcasts for it. Classify by downcasting, never by matching message text — the same rule `konnect_ipc::TransportUnreachable` follows.

A path that is *present but unusable* — absent on disk, wrong extension — is deliberately **not** an argument error. That is the handler trying and failing, and it stays a `handler_error` or `FileNotFound`. Collapsing the two would make "you forgot an argument" and "that file is not there" indistinguishable.

Why the gate exists: nothing validated `required` server-side, and `execute_tool` turns absent arguments into `{}`, so 25 sites across 18 tools read a required argument with `unwrap_or` and reported success on a substituted value (#218). Each is now fixed in its own handler; the gate is the floor beneath them. Guarded by `every_tool_enforces_its_required_arguments`, which calls every tool that declares a required argument with no arguments at all — safe to be exhaustive precisely because the gate refuses before any handler runs.

Most in-handler errors still use `CallToolResult::error("free text")` or bubble `anyhow::Error`; migrating them is incremental. `project.rs::handle_get_project_info` demonstrates the structured `FileNotFound` pattern.

### One value KiCAD does not reject: an unrepresentable layer

`builders::layer_from_name` maps a layer name to a `BoardLayer`, and anything it does not know becomes `BL_UNDEFINED`. KiCAD 10.0.5 does **not** validate that field on an incoming item — it indexes its layer bitset with whatever arrives — so `BL_UNDEFINED` is answered with an access violation that terminates the process and takes the user's unsaved board with it (#237). Konnect sees only an NNG receive timeout.

So on any path that puts a layer into a message bound for KiCAD, use **`builders::try_layer_from_name`**, which refuses instead. `client.rs::build_footprint_item` validates the root layer, every pad layer and every graphic layer before it builds a single child; the `*.Cu`/`*.Mask`/`*.Paste` wildcards KiCAD itself writes are expanded rather than mapped, so they are skipped.

The infallible `layer_from_name` stays for read paths that already filter `BL_UNDEFINED` out. If you add a write path, the fallible one is the default — this is the class of bug where "the tool returned no error" and "the editor is still alive" are different questions.

## Observability

Every `tools/call` flows through `McpHandler::execute_tool`, which wraps the dispatch with:
- A **ring buffer** of the last 100 `CallRecord`s (surfaced via `get_recent_calls` meta-tool).
- **Per-tool counters** for totals, errors, cumulative duration, last-status, last-error (surfaced via `server_stats`).
- **JSONL append** to `<konnect dir>/logs/calls.jsonl` (one line per call). Paths:
  - Windows: `%APPDATA%\konnect\logs\calls.jsonl`
  - macOS: `~/Library/Application Support/konnect/logs/calls.jsonl`
  - Linux: `~/.konnect/logs/calls.jsonl`
- **Structured `tracing` events** (`tool_call_start` + `tool_call_end`) carrying `call_id`, `tool`, `toolset`, `status`, `dur_ms` — greppable in the stderr log.

Each `CallRecord` includes: `call_id`, `ts` (unix ms), `tool`, `toolset` (optional — `None` for meta-tools), `dur_ms`, `status` (`ok` / `error` / `not_found`), `error_kind`, `args_bytes`, `result_bytes`.

The observer is constructed once by `McpHandler::new` and stashed on both the handler and `ToolContext` so meta-tools can reach it. IO failures on the JSONL file never fail the tool call — they `tracing::warn!` and are silently dropped. Tests construct an in-memory-only observer via `ToolContext::new(...)` (no `log_path`).

Source: [`crates/konnect-core/src/observability.rs`](crates/konnect-core/src/observability.rs).

## Tool Routing (Starter Kit + On-Demand Loading)

The server does NOT expose all 221 tools (228 total with the 7 meta-tools) in `tools/list` by default — that would cost ~23K tokens of context on every listing. Instead:

- **Startup**: only `STARTER_KIT` toolsets are pre-loaded (see `router/registry.rs::STARTER_KIT`). Currently: `project`, `config`. Combined with the 7 meta-tools, baseline `tools/list` is 21 tools ≈ 2K tokens.
- **On demand**: the LLM reads `list_toolboxes` → calls `load_toolset(name)` to expose a toolset's tools in subsequent `tools/list` responses. `unload_toolset(name)` prunes them when the task shifts.
- **`tools/list_changed` notification**: sent on every load/unload so MCP clients refresh their local tool cache.
- **Error recovery**: if the LLM calls an unloaded tool, `handler.rs` returns an actionable error naming the toolset that owns it (so the LLM can load it and retry in one hop — no extra `list_toolboxes` round-trip).
- **`auto_load_toolsets` (config key, default `false`)**: when set, a miss in `dispatch_tool` loads the owning toolset and executes the call in the same hop instead of returning `toolset_not_loaded` -- fewer round trips, at the cost of toolsets accumulating monotonically for the rest of the session (`unload_toolset` still prunes, but a tool call reloads its toolset right back). Off by default because the router's whole point is keeping `tools/list` small; turn it on only if your client would rather eat the context growth than handle one recoverable error per miss. Set via `konnect.toml`/`settings.json` (`auto_load_toolsets = true`) or the equivalent `ServerConfig` field when embedding.

- **`eager_toolsets` (config key, default `false`)**: pre-loads every toolset at startup via `ToolRouter::load_all`, so the *first* `tools/list` is the full catalogue. This is for MCP clients that cache the initial tool list and never act on `notifications/tools/list_changed` — for those, a tool absent from the first listing can never be called at all, because `load_toolset` reports the names it loaded but returns no schemas and the client has nothing to invoke (#134, #169). Note `auto_load_toolsets` does **not** cover this case: it fires on a tool *call*, so it only helps a caller that already knows the tool name. Costs ~25K tokens per listing against the ~2K baseline, which is the router's entire reason for existing — hence off by default.

The router is defined in `crates/konnect-core/src/router/mod.rs`.

## Build Requirements

- Rust toolchain pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (currently 1.96.0) —
  rustup picks it up automatically, and CI compiles with the same version. The pinned
  version IS the MSRV: bump it deliberately, in its own commit, after running the full
  local gate on the new version.
- `protoc` binary (for protobuf code generation in konnect-ipc crate)
  - Set `PROTOC` to the binary, or leave it unset and `konnect-ipc/build.rs` resolves
    `protoc` from PATH
  - Well-known-type includes (`google/protobuf/any.proto`) are derived from
    `<protoc>/../../include` after the binary is resolved to an absolute path. This
    covers both the upstream release layout (`bin/protoc` beside `include/`) and system
    packages (`/usr/bin/protoc` → `/usr/include`). Set `PROTOC_INCLUDE` to override when
    the binary and its protos live in unrelated prefixes — notably Chocolatey and scoop,
    whose shared shim directory is not the package prefix.
  - Some distributions ship the well-known `.proto` files separately from the compiler.
    Debian/Ubuntu need `protobuf-compiler` **and** `libprotobuf-dev`; Fedora needs
    `protobuf-compiler` and `protobuf-devel`. Missing them fails the build with
    `google/protobuf/any.proto: File not found`.
  - Download: https://github.com/protocolbuffers/protobuf/releases
- For schematic-viewer (built separately from the workspace — see Quick Start):
  - Rust toolchain on PATH (Windows: `set PATH=%PATH%;%USERPROFILE%\.cargo\bin` if `cargo`
    isn't recognized in the shell)
  - Tauri 2 prerequisites: WebView2 runtime on Windows (usually pre-installed on Win 10/11)
  - At runtime it discovers `kicad-cli` from the standard KiCAD install paths, then PATH;
    override with `--kicad-cli <path>`
  - Rebuilds fail while a viewer window is open (Windows locks the running `.exe`) — close
    the app before `cargo build`

## Test Suite

Run all: `PROTOC=<path> cargo test --workspace --lib --tests`

| Location | What |
|----------|------|
| `konnect-sexp` unit tests | Parser, writer, geometry transforms |
| `konnect-core` unit tests | Router load/unload, starter-kit, registry invariants, observability, error taxonomy, arg helpers |
| `konnect-core` integration tests | Fixture files: parse, edit, write, observability, structured errors |
| `konnect-schematic-editor` tests | Typed schematic model + round-tripping |
| `konnect-ipc` unit tests | Protobuf builders, client message construction, rigid-body child transform |
| `crates/konnect/tests/` | Protocol over stdio, doc/asset count guards, transaction CLI, `#[ignore]`d live-KiCAD tests |

`schematic-viewer` is **excluded from the workspace** (`Cargo.toml`'s `[workspace] exclude`) since
it's a Tauri app built separately, so `cargo test --workspace` never touches it. **CI does**: the
dedicated `viewer` job in `.github/workflows/ci.yml` runs `cargo check` and `cargo test` against
`crates/schematic-viewer/Cargo.toml`, because without it a viewer break ships silently. Run its
tests locally with `cd crates/schematic-viewer && cargo test`. Its 20 unit tests cover the pure sheet-tree-walking,
watch-directory, render-snapshot, event-debounce, and incremental-render-selection logic
(`walk_sheet_tree`, `compute_watch_dirs`, `snapshot_tree`, `drain_until_quiet`,
`files_needing_render`, `render_all`'s error handling) — the actual `kicad-cli` subprocess call
and Tauri command/event plumbing stay thin and untested, matching this codebase's existing
convention for other `kicad-cli`-calling code.

## Adding a New Tool

1. Add the `tool!(...)` definition to the appropriate toolset's `tools()` vec
2. Write the `async fn handle_*()` handler below the tools vec
3. Update `tool_count` in `router/registry.rs::ALL_TOOLSETS` — this is the declared count shown in `list_toolboxes`
4. If the new tool belongs in the default-available set, add its toolset to `registry.rs::STARTER_KIT`
5. Run `cargo check` and re-run the tool-directory extraction (see `tool-directory.md` header) to keep the docs in sync

## Current Stats

- **20 toolsets, 221 tools** + 7 meta-tools (4 routing + 2 observability + 1 runtime diagnostic — see `tool-directory.md`)
- Baseline `tools/list`: 21 tools / ~2K tokens (starter kit + meta-tools)
- Full-catalog `tools/list` (all loaded): 228 tools (221 registered + 7 meta) / ~25K tokens
- **0 IPC stubs** (all protobuf methods implemented)
- **0 unimplemented tools**
- **Specctra DSN/SES are PCB-editor operations**, not `kicad-cli` commands.
  `export_specctra_dsn` creates a revision-bound routing job from the live
  editor, `route_specctra_dsn` delegates the route to Freerouting's local native
  MCP server, and `plan_specctra_ses_import` / `apply_specctra_ses` validate and
  return the result through one KiCad undo transaction.
