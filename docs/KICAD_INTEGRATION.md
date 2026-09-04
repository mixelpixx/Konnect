# KiCad Integration

Konnect uses direct KiCad file editing, KiCad IPC, and `kicad-cli`. The correct
path depends on the operation and whether KiCad currently owns the document.

## Schematic File Editing

Schematic handlers under `crates/konnect-core/src/tools/sch_*.rs` operate on
saved `.kicad_sch` files through `konnect-schematic-editor` and
`konnect-sexp`. These paths work without a running KiCad process and preserve
the embedded library definitions, UUIDs, and instance information needed by
KiCad.

Existing-file writes use the atomic/conflict-aware machinery in
`konnect-sexp/src/writer.rs`. Multi-file changes use
`konnect-sexp/src/transaction.rs`; a source revision change must become a
conflict rather than an overwrite.

## KiCad IPC

`crates/konnect-ipc` sends typed protobuf requests over NNG. The socket comes
from `ipc_address` or `KICAD_API_SOCKET`; KiCad-provided credentials such as
`KICAD_API_TOKEN` are carried in the IPC client request metadata.

The three board-write gates are:

- `KiCadIpcClient::ensure_board_is_active` in `konnect-ipc/src/client.rs`
  prevents a request naming one board from changing another open board.
- `attempt_ipc_write` in `konnect-core/src/tools/pcb_board.rs` permits a file
  fallback only when IPC is unreachable. A response from KiCad, including a
  rejection, fails closed.
- `refuse_if_board_open_in_kicad` in the same module protects file-only tools
  from edits KiCad would discard on its next save.

Closed-board move, rotate, and flip in `tools/pcb_components.rs` are narrowly
scoped exceptions with explicit geometry checks. They are not a general license
to edit a live board file.

## Schematic-To-Board Sync

`update_pcb_from_schematic` in `tools/pcb_sync.rs` is live-IPC-only. It uses
`tools/cli.rs` to export a netlist from the saved hierarchy, plans against a live
snapshot, requires the current plan revision for apply, performs one IPC commit,
and reads the affected footprint shapes back.

The read-back is a correctness boundary, not merely a diagnostic convenience.
In earlier releases, protobuf `Any` values carrying footprint graphics decoded
as empty pads because unknown proto fields are skipped; KiCad accepted the
mutation. The v0.7 path discriminates the declared type and verifies the board
after commit.

## `kicad-cli`

`crates/konnect-core/src/tools/cli.rs` is the shared subprocess and result parser
for ERC, DRC, exports, and rendering. Callers should use it rather than build
ad-hoc command lines.

The DRC result model preserves design-rule violations, unconnected items, and
schematic parity. `verification.rs`, `pcb_export.rs`, `design_review.rs`, and
`manufacturing.rs` consume that complete result; unavailable categories or a
failed CLI run cannot be treated as a clean board.

Konnect's Freerouting bridge keeps the KiCad and routing responsibilities
separate: `export_specctra_dsn` snapshots the live board and writes a
revision-bound DSN job, `route_specctra_dsn` drives the discovered local JAR
through Freerouting's native headless MCP server, and
`plan_specctra_ses_import` / `apply_specctra_ses` validate and apply the result
through one KiCad undo transaction. Board data stays local, output files are
created without replacement, and the Freerouting child process is bounded and
owned by Konnect.

On KiCad 10, `export_specctra_dsn` can optionally use the legacy Python
ActionPlugin as a deliberately narrow native-export bridge. The plugin calls
KiCad's own `pcbnew.ExportSpecctraDSN` on the UI thread and returns a
plugin-owned temporary file over an authenticated loopback endpoint. Rust
still captures the immutable IPC snapshot, rejects a board revision change,
checks that the native DSN has the same components, pads, nets, layers, and
routing rules, and writes the revision-bound reverse manifest. The temporary
file is consumed and deleted.

The `native_bridge_mode` tool argument controls selection: `prefer` (the default)
uses a running bridge and otherwise falls back to the Rust DSN exporter,
`require` fails if native export cannot be used, and `disable` uses Rust only.
Native export is disabled in the plugin settings by default. This bridge is a
KiCad 10 compatibility path, not a substitute for the executable IPC plugin or
the KiCad 11 architecture; strict SES planning and atomic IPC apply never pass
through Python.

## Configuration

Konnect selects the first existing server configuration file in the order below
and loads only that file. It does not merge settings from later locations. Call
`get_installation_info` to see which file configured the running process and
which later files were shadowed.

So a value present only in a lower-priority file has no effect while a
higher-priority file exists.

1. `konnect.toml` in the working directory;
2. `settings.json` in the working directory;
3. `settings.json` beside the executable;
4. `settings.json` one directory above the executable;
5. the platform configuration directory.

Two consequences worth stating outright, because both have been reported as
missing functionality:

- In the standard plugin layout a `settings.json` exists beside the binary, so
  the platform configuration directory is never reached and any `config.toml`
  there is inert.
- A stray `konnect.toml` or `settings.json` in a working directory takes over the
  entire configuration for that run.

Call **`get_installation_info`** to see which file configured the running
process: its `configuration` block reports `source`
(`explicit_path` | `search_path` | `defaults`), the absolute `selected_path`, the
`search_policy`, and `skipped_existing_paths` — the later files that exist and
were shadowed. The values are captured at startup, so a file created afterwards
is not reported. The server also logs one INFO line naming the selection at
startup, for when no tool call is possible yet.

Note that `load_user_config` is a different plane: it reads a user
design-preferences file (`config.json`), not the server startup configuration
described here, so its path does not answer "which file configured the server".

`--config <path>` loads an explicit file and bypasses the search entirely; the
list above is not consulted, and `get_installation_info` reports
`source: "explicit_path"` with no skipped candidates. Relevant fields include `kicad_cli`,
`kicad_binary`, `ipc_address`, `transport`, `http_address`, `jlcpcb_db_path`,
`log_level`, `auto_load_toolsets`, and `eager_toolsets`. The legacy
`ipc_socket_path` alias is accepted by the serde definition in `config.rs`.

## Plugin, Viewer, And Packaging

`plugin` contains the legacy KiCad 10 Python ActionPlugin for settings/server
control and the optional native Specctra bridge. `plugin.json` declares the
separate executable IPC integration that is the forward path. The standalone viewer in
`crates/schematic-viewer` watches schematic files and renders through
`kicad-cli`; it is built and tested separately from the Rust workspace.

The PCM assembly scripts in `packaging/build-pcm.ps1` and `build-pcm.sh` stage
only metadata, plugin files, icons, and binaries. Repository developer
documentation under `docs/` is intentionally not part of the install zip.
