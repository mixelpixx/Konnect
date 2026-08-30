# Architecture

Konnect separates process concerns, MCP/tool behavior, and KiCad data access.
This page describes both those static boundaries and the runtime flows through
them. `DEV.md` remains the deeper implementation reference.

## Static Boundaries

### Server process: `crates/konnect`

`crates/konnect/src/main.rs` classifies help, version, installer, transaction,
and server invocations before any server-side writes occur. It loads
`Config`, initializes tracing on stderr, constructs `McpHandler`, and selects
stdio, HTTP, or both transports.

`crates/konnect/src/config.rs` owns TOML/JSON discovery and the environment
fallback for `KICAD_API_SOCKET`. The transport implementations live in
`crates/konnect/src/transport/stdio.rs` and `http.rs`. Client guidance
installation is owned by `crates/konnect/src/install.rs`; schematic transaction
inspection and recovery commands are owned by `transaction_cli.rs`.

### MCP and domain logic: `crates/konnect-core`

`crates/konnect-core/src/mcp/handler.rs` handles MCP methods, enforces required
argument presence, dispatches tool calls, emits tool-list notifications, and
records calls through `observability.rs`.

`crates/konnect-core/src/router/registry.rs` declares toolsets and resolves each
toolset to its definitions. `router/mod.rs` tracks loaded definitions, and
`router/meta_tools.rs` implements the always-visible discovery, loading, and
observability tools. `runtime_info.rs` supplies the read-only serving-build,
installation, KiCad, and IPC evidence returned by `get_installation_info`.

`crates/konnect-core/src/tools/mod.rs` owns `ToolDef`, `ToolContext`,
`ServerConfig`, the `tool!` macro, required-argument helpers, and shared path and
library helpers. Domain handlers live in the other files under `src/tools`.

### KiCad data layers

`crates/konnect-sexp` owns low-level KiCad S-expression parsing, edits, geometry,
layer/net primitives, atomic replacement, reversible commands, and multi-file
transaction journals. It does not require a live KiCad process.

`crates/konnect-schematic-editor` owns the typed schematic model, library symbol
resolution, and parse-mutate-serialize workflows for symbols, wires, labels,
sheets, and related schematic concepts.

`crates/konnect-ipc` owns the KiCad IPC client, generated protobuf types, NNG
request/reply transport, typed board operations, and the distinction between an
unreachable transport and a request KiCad received and rejected.

### Non-workspace and non-Rust boundaries

`crates/schematic-viewer` is a separate Tauri application and is deliberately
outside the Cargo workspace. `plugin` is the thin Python KiCad integration
layer. `packaging` contains the PCM build scripts, metadata, schema, and package
validation. Bundled AI guidance under `crates/konnect/assets/skills` and
`assets/agents` is user-facing workflow behavior and must track the tools it
names.

## Server Startup

```text
crates/konnect/src/main.rs
  -> classify invocation
  -> Config::load or Config::load_from
  -> McpHandler::new
  -> ToolRouter with starter toolsets (or every toolset when configured)
  -> stdio and/or HTTP transport
```

`McpHandler::new` in `mcp/handler.rs` uses the `STARTER_KIT` declared by
`router/registry.rs` unless `eager_toolsets` requests a complete initial list.
The latter exists for clients that do not refresh after
`notifications/tools/list_changed`; the configuration contract is documented
beside the fields in `crates/konnect/src/config.rs`.

## Listing, Loading, And Calling Tools

```text
tools/list
  -> McpHandler::dispatch
  -> meta_tools::meta_tool_descriptions
  -> ToolRouter::active_tools

tools/call load_toolset
  -> router/meta_tools.rs
  -> ToolRouter::load
  -> notifications/tools/list_changed

tools/call domain_tool
  -> McpHandler::execute_tool
  -> meta-tool or loaded domain definition
  -> required-field gate
  -> handler
  -> CallToolResult and observability record
```

`load_toolset` accepts one name or an array of names in
`router/meta_tools.rs`. If an unloaded tool is called, `mcp/handler.rs` either
auto-loads its owner when configured or returns a structured
`toolset_not_loaded` error. Stdio delivers list-change notifications through
`transport/stdio.rs`; HTTP delivers them through the SSE path in
`transport/http.rs`.

## Schematic File Mutation

Schematic handlers in `tools/sch_*.rs` read a saved schematic, mutate it through
`konnect-schematic-editor` or `konnect-sexp`, and commit with revision-aware
atomic replacement. If the source changes after it is read, the writer must
return a conflict instead of overwriting it.

Multi-file schematic changes use the write-ahead journal in
`konnect-sexp/src/transaction.rs`. The `konnect transaction` commands in
`crates/konnect/src/transaction_cli.rs` inspect, recover, or explicitly abandon
those journals.

## Board Mutation And Write Gates

Board writes use three distinct patterns:

1. Live IPC handlers call `KiCadIpcClient::ensure_board_is_active` in
   `konnect-ipc/src/client.rs` before changing the editor document.
2. Hybrid handlers use `attempt_ipc_write` in
   `konnect-core/src/tools/pcb_board.rs`. Only an unreachable IPC transport may
   fall back to a file edit; a reached-and-rejected request fails closed.
3. File-only board handlers call `refuse_if_board_open_in_kicad` in
   `tools/pcb_board.rs` so KiCad cannot later overwrite an invisible file edit.

Closed-board move, rotate, and flip behavior is implemented in
`tools/pcb_components.rs`. These paths preserve rigid-body footprint geometry
and refuse inputs whose geometry cannot be transformed safely.

`update_pcb_from_schematic` is a special live-only path in `tools/pcb_sync.rs`.
It exports the saved schematic hierarchy through the CLI wrapper, creates a dry
run plan, requires that plan's revision for apply, performs one IPC commit, and
reads the result back. The read-back check exists because a prior protobuf type
mistake converted footprint graphics into pads while every layer reported
success.

## Checks, Exports, And Verdicts

`crates/konnect-core/src/tools/cli.rs` is the subprocess wrapper for KiCad CLI
checks and exports. Domain-facing handlers in `verification.rs`,
`pcb_export.rs`, `sch_export.rs`, `manufacturing.rs`, and `design_review.rs`
interpret those results.

Positive review and manufacturing verdicts are evidence-gated. In particular,
the DRC model in `tools/cli.rs` preserves KiCad's design-rule,
unconnected-item, and schematic-parity categories; `design_review.rs` and
`manufacturing.rs` treat unavailable evidence as incomplete rather than clean.

## Ownership Rules

- CLI lifecycle, configuration, installer behavior, and transports belong in
  `crates/konnect`.
- MCP protocol handling, routing, tool responses, and observability belong in
  `crates/konnect-core`.
- Schematic and general KiCad file primitives belong in `konnect-sexp` or
  `konnect-schematic-editor`.
- Live board IPC behavior and transport classification belong in `konnect-ipc`;
  policy for using it belongs in the calling `konnect-core` handler.
- KiCad plugin and PCM changes belong in `plugin` and `packaging`.
