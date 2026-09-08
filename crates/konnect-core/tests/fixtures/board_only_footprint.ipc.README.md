# `board_only_footprint.ipc.bin`

A single `kiapi.board.types.FootprintInstance`, serialised exactly as KiCad's
IPC API sent it, for a **board-only footprint** — one placed directly on the
board with no schematic symbol behind it.

Decoded by `pcb_sync`'s regression tests for issue #452, where the defect was
the reader turning the *absence* of a schematic identity into the shared
identity `/`.

## Provenance

| | |
|---|---|
| KiCad | 10.0.6, macOS (Apple Silicon) |
| Source board | `crates/konnect-ipc/tests/fixtures/live_ipc.kicad_pcb` — KiCad's own GPL EuroCard160mmX100mm template |
| Item | `MH1`, one of the template's four `MountingHole:MountingHole_2.7mm_M2.5_DIN965` holes, none of which carries a `(path ...)` line |
| Transport | `GetItems` over the running editor's IPC socket |
| Captured | 2026-09-08 |
| Size | 1797 bytes |

## Regenerating it

Set `KONNECT_CAPTURE_IPC_FIXTURE=1` when running the ignored live test
`kicad_reports_an_empty_sheet_path_for_a_board_only_footprint` in
`crates/konnect-ipc/tests/live_kicad_test.rs`. That test also *verifies* the
property this capture exists to encode, so a regeneration that changes the
answer fails rather than silently rewriting the fixture.

The live test needs a running KiCad with its API enabled, the board above open,
and `KICAD_API_SOCKET` pointing at the **PCB editor's** socket. Resolve that by
matching `api-<pid>.sock` to a live `pcbnew` process: a stale socket from a
closed session can be newer by mtime than the live one.

## Redaction

Nothing was removed. The message was checked for filesystem paths, usernames,
hostnames and the board filename before being committed; it contains only
KiCad's stock `MountingHole` library strings, standard field names, protobuf
type URLs and KIIDs from the template board. `symbol_sheet_name` and
`symbol_sheet_filename` — the two fields that could carry a path — are empty
strings on a board-only footprint, which is why there is nothing to redact.

## What it records that a hand-written fixture got wrong

Two things, both corrected by this capture:

1. `symbol_path` is **present and empty** — `Some(SheetPath { path: [] })`,
   with `path_human_readable: ""`, not `"/"` and not `None`. The whole #452 fix
   rests on that distinction.
2. `not_in_schematic` is **false**. KiCad marks a board-only mounting hole
   `exclude_from_position_files` and `exclude_from_bill_of_materials` instead.
   The hand-written fixture this replaced set that flag true, which made the
   planner's rename-ambiguity branch unreachable from the tests written to
   cover it.
