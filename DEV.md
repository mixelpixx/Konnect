# Developer Guide — Konnect

Internal reference for developing and maintaining the Rust port.

## Quick Start

```bash
# Required: protoc for protobuf code generation
set PROTOC=C:\path\to\protoc.exe   # or install via `choco install protoc`

cargo check                          # verify everything compiles (~15s)
cargo test --workspace --lib --tests # all tests
cargo build --release -p konnect # build the MCP server binary

# Build the schematic viewer (separate crate)
cd crates/schematic-viewer
cargo build --release
```

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
│   ├── konnect-core/          # All tool logic (18 toolsets)
│   │   └── src/
│   │       ├── mcp/
│   │       │   ├── protocol.rs      # MCP JSON-RPC 2.0 types
│   │       │   ├── handler.rs       # Dispatch: initialize, tools/list (all tools static), tools/call
│   │       │   └── server.rs        # Session state machine
│   │       ├── router/
│   │       │   ├── mod.rs           # ToolRouter: load/unload toolsets
│   │       │   ├── registry.rs      # Static toolset metadata + tools_for() dispatcher
│   │       │   └── meta_tools.rs    # 4 always-visible meta-tools
│   │       └── tools/
│   │           ├── mod.rs            # ToolDef, ToolContext, tool! macro, helpers, kicad_config_dir(), resolve_lib_symbol()
│   │           ├── cli.rs            # kicad-cli v10 subprocess wrapper (verified against actual binary)
│   │           ├── project.rs        # 6 tools (incl. open_schematic_viewer)
│   │           ├── sch_components.rs # 17 tools (component placement with lib_symbols embedding)
│   │           ├── sch_wiring.rs     # 19 tools (incl. connect_pins, power symbol embedding)
│   │           ├── sch_analysis.rs   # 15 tools (union-find net graph, connectivity)
│   │           ├── sch_batch.rs      # 10 tools (single-read/single-write atomic operations)
│   │           ├── sch_export.rs     # 7 tools (SVG/PDF/netlist/ERC)
│   │           ├── sch_hierarchy.rs  # 7 tools (typed Sheet model, add/edit/move/delete/duplicate + hierarchy/page queries)
│   │           ├── pcb_board.rs      # 10 tools (S-expr file editing, IPC fallback)
│   │           ├── pcb_components.rs # 13 tools (IPC real-time via NNG+protobuf)
│   │           ├── pcb_routing.rs    # 12 tools (traces, vias, nets, netclasses)
│   │           ├── pcb_export.rs     # 9 tools (Gerber, PDF, 3D, DRC)
│   │           ├── library.rs        # 14 tools (symbol/footprint library management)
│   │           ├── integration.rs    # 11 tools (JLCPCB SQLite, Freerouting, datasheets)
│   │           ├── verification.rs   # 8 tools (DRC, design rules, KiCAD UI)
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
│   │       └── geometry.rs           # PinTransform, transform_pin (CANONICAL pin math)
│   │
│   ├── konnect-ipc/                   # KiCAD 10 IPC API client
│   │   ├── proto/                    # Protobuf definitions (copied from KiCAD v10 source)
│   │   ├── build.rs                  # prost-build protobuf code generation
│   │   └── src/
│   │       ├── gen.rs                # Generated protobuf Rust types
│   │       ├── client.rs             # NNG req/rep client, all methods implemented
│   │       ├── builders.rs           # Protobuf message construction helpers (mm→nm conversion)
│   │       └── types.rs              # Public types (IpcFootprint, IpcTrack, etc.)
│   │
│   └── schematic-viewer/            # Tauri desktop app (separate from workspace)
│       ├── tauri.conf.json
│       ├── src/main.rs               # File watcher + kicad-cli SVG rendering + Tauri commands
│       └── frontend/index.html       # Pan/zoom SVG viewer with auto-refresh
│
├── plugin/                           # Python thin launcher (runs inside KiCAD)
│   ├── __init__.py                   # pcbnew.ActionPlugin — settings dialog (PCB Editor only)
│   ├── settings_dialog.py            # wxPython settings UI (paths, server control)
│   └── plugin.json                   # KiCAD 10 IPC plugin manifest
│
├── packaging/
│   └── metadata.json                 # KiCAD PCM package manifest
│
└── .github/workflows/
    ├── ci.yml                        # Check + test + clippy on 3 platforms
    └── release.yml                   # Build binaries + GitHub Release on tag push
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
- All edits use `write_atomic` (write to .tmp → fsync → rename)

### kicad-cli v10 (Subprocess)
- Verified commands: `sch erc`, `sch export svg/pdf/bom/netlist`, `pcb drc`, `pcb export gerbers/drill/pdf/svg/step/vrml/pos/ipcd356`, `pcb render`
- Removed in v10: `sch annotate` (reimplemented in Rust), `pcb sync`, `pcb export/import specctra`
- Version format: `20250610`

### Plugin Installation
- **PCM zip** is the correct install method
- KiCAD installs to: `C:\KiCad\10.0\share\kicad\scripting\plugins\konnect\`
- Both `__init__.py` (SWIG ActionPlugin for PCB editor settings dialog) and `plugin.json` (IPC exec plugin) are included

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
| `file_not_found` | Referenced file doesn't exist |
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

The dispatch-level errors (not-loaded/unknown/handler-panic) are fully structured. So are **all missing-argument errors** across all 171 tools — `tools/mod.rs::require_str` / `require_f64` emit `ToolErrorKind::InvalidArgument { field, reason }` automatically. Most in-handler errors still use `CallToolResult::error("free text")` or bubble `anyhow::Error`; migrating them is incremental. `project.rs::handle_get_project_info` demonstrates the structured `FileNotFound` pattern.

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

The server does NOT expose all 171 tools in `tools/list` by default — that would cost ~23K tokens of context on every listing. Instead:

- **Startup**: only `STARTER_KIT` toolsets are pre-loaded (see `router/registry.rs::STARTER_KIT`). Currently: `project`, `config`. Combined with the 4 meta-tools, baseline `tools/list` is ~17 tools ≈ 2K tokens.
- **On demand**: the LLM reads `list_toolboxes` → calls `load_toolset(name)` to expose a toolset's tools in subsequent `tools/list` responses. `unload_toolset(name)` prunes them when the task shifts.
- **`tools/list_changed` notification**: sent on every load/unload so MCP clients refresh their local tool cache.
- **Error recovery**: if the LLM calls an unloaded tool, `handler.rs` returns an actionable error naming the toolset that owns it (so the LLM can load it and retry in one hop — no extra `list_toolboxes` round-trip).

The router is defined in `crates/konnect-core/src/router/mod.rs`.

## Build Requirements

- Rust 1.75+ (stable)
- `protoc` binary (for protobuf code generation in konnect-ipc crate)
  - Set `PROTOC` environment variable or install on PATH
  - Download: https://github.com/protocolbuffers/protobuf/releases
- For schematic-viewer: Tauri 2 prerequisites (WebView2 on Windows — usually pre-installed)

## Test Suite

Run all: `PROTOC=<path> cargo test --workspace --lib --tests`

| Location | What |
|----------|------|
| `konnect-sexp` unit tests | Parser, writer, geometry transforms |
| `konnect-core` unit tests | Router load/unload, starter-kit, registry invariants, observability, error taxonomy, arg helpers |
| `konnect-core` integration tests | Fixture files: parse, edit, write, observability, structured errors |
| `konnect-schematic-editor` tests | Typed schematic model + round-tripping |

## Adding a New Tool

1. Add the `tool!(...)` definition to the appropriate toolset's `tools()` vec
2. Write the `async fn handle_*()` handler below the tools vec
3. Update `tool_count` in `router/registry.rs::ALL_TOOLSETS` — this is the declared count shown in `list_toolboxes`
4. If the new tool belongs in the default-available set, add its toolset to `registry.rs::STARTER_KIT`
5. Run `cargo check` and re-run the tool-directory extraction (see `tool-directory.md` header) to keep the docs in sync

## Current Stats

- **18 toolsets, 182 tools** + 6 meta-tools (4 routing + 2 observability — see `tool-directory.md`)
- Baseline `tools/list`: ~19 tools / ~2K tokens (starter kit + meta-tools)
- Full-catalog `tools/list` (all loaded): ~188 tools / ~24K tokens
- **0 IPC stubs** (all protobuf methods implemented)
- **0 unimplemented tools**
- **3 CLI commands removed in KiCAD v10** (specctra DSN/SES, pcb sync — return clear errors)
