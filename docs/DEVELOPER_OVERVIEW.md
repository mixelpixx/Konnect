# Developer Overview

This is the map layer for developers who need to understand Konnect before
changing it. Detailed implementation notes and the authoritative current tool
statistics remain in [`DEV.md`](../DEV.md); public naming and contribution rules
remain in [`CONTRIBUTING.md`](../CONTRIBUTING.md) and
[`NAMING_CONVENTIONS.md`](NAMING_CONVENTIONS.md).

Konnect is a Rust MCP server with KiCad integration and packaging support. An
MCP client talks to the `konnect` binary, which exposes a small starter surface,
loads domain toolsets on demand, and routes each call to a file, IPC, or
`kicad-cli` backend.

## Main Components

| Component | Source | Responsibility |
|---|---|---|
| Server binary | `crates/konnect` | CLI classification, configuration, installer/status commands, MCP transports, and plugin FFI exports |
| Core tool server | `crates/konnect-core` | MCP handling, routing, observability, tool definitions, and domain handlers |
| S-expression engine | `crates/konnect-sexp` | KiCad file parsing/writing, atomic edits, reversible commands, and transaction journals |
| Typed schematic editor | `crates/konnect-schematic-editor` | Higher-level schematic parse, mutation, library resolution, and serialization |
| KiCad IPC client | `crates/konnect-ipc` | Typed KiCad IPC messages, NNG transport, board targeting, and failure classification |
| Schematic viewer | `crates/schematic-viewer` | Separate Tauri application for live schematic rendering |
| KiCad launcher plugin | `plugin` | Python ActionPlugin and settings UI for the packaged server |
| Packaging | `packaging` | KiCad PCM metadata, package assembly, and package validation |

## Runtime Shape

The main ownership flow is:

```text
MCP client
  -> transport in crates/konnect/src/transport
  -> McpHandler in crates/konnect-core/src/mcp/handler.rs
  -> ToolRouter in crates/konnect-core/src/router
  -> handler in crates/konnect-core/src/tools
  -> one of:
       schematic model or S-expression file edit
       KiCad IPC request
       guarded closed-board file edit
       kicad-cli check or export
       external data/integration operation
```

The backend is selected per operation, not merely by file type. The write-path
decision and its safety gates are described in
[`KICAD_INTEGRATION.md`](KICAD_INTEGRATION.md) and implemented principally in
`tools/pcb_board.rs`, `tools/pcb_components.rs`, `tools/pcb_sync.rs`, and
`konnect-ipc/src/client.rs`.

## Reading Order

1. [Architecture](ARCHITECTURE.md) covers static ownership and the dynamic
   startup, routing, mutation, and recovery flows.
2. [Tool system](TOOL_SYSTEM.md) covers definitions, toolsets, dispatch,
   argument contracts, response evidence, and structured errors.
3. [Developing tools](DEVELOPING_TOOLS.md) is the implementation checklist for
   adding or changing a tool safely.
4. [KiCad integration](KICAD_INTEGRATION.md) explains the file, IPC, and CLI
   paths and the gates between them.
5. [Testing and release](TESTING_AND_RELEASE.md) maps changes to tests, CI, live
   KiCad validation, documentation, and packaging.
