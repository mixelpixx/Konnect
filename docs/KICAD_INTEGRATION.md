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

Freerouting remains usable through its KiCad ActionPlugin, but Konnect does not
currently have a safe DSN/SES bridge. The `autoroute` behavior in
`tools/integration.rs` reports that limitation instead of claiming KiCad removed
CLI commands that never existed.

## Configuration

`crates/konnect/src/config.rs` searches, in order:

1. `konnect.toml` in the working directory;
2. `settings.json` in the working directory;
3. `settings.json` beside the executable;
4. `settings.json` one directory above the executable;
5. the platform configuration directory.

`--config <path>` loads an explicit file. Relevant fields include `kicad_cli`,
`kicad_binary`, `ipc_address`, `transport`, `http_address`, `jlcpcb_db_path`,
`log_level`, `auto_load_toolsets`, and `eager_toolsets`. The legacy
`ipc_socket_path` alias is accepted by the serde definition in `config.rs`.

## Plugin, Viewer, And Packaging

`plugin` is a thin Python KiCad integration layer that launches/configures the
Rust server included in a PCM bundle. The standalone viewer in
`crates/schematic-viewer` watches schematic files and renders through
`kicad-cli`; it is built and tested separately from the Rust workspace.

The PCM assembly scripts in `packaging/build-pcm.ps1` and `build-pcm.sh` stage
only metadata, plugin files, icons, and binaries. Repository developer
documentation under `docs/` is intentionally not part of the install zip.
