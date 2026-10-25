# Specctra two-resistor board fixture

`specctra_two_resistors.kicad_pcb` is a deliberately small two-layer board used
to test the first fail-closed Specctra export profile. It was derived from the
repository's existing PCB integration fixture, assigned stable test UUIDs and a
closed rectangular outline, then opened and re-saved by KiCad 10.0.5 with:

```text
kicad-cli pcb upgrade --force specctra_two_resistors.kicad_pcb
```

That final KiCad-authored serialization is intentional. In particular, it
captures KiCad 10's direct `(net "NAME")` pad syntax rather than relying on a
hand-written approximation of the board format.

`specctra_two_resistors.native-kicad-10.dsn` was exported from that board by
KiCad 10.0.5's real `pcbnew.ExportSpecctraDSN(board, path)` binding. Only the
environment-specific absolute output path in the root `(pcb ...)` identifier
was normalized to `board.dsn`; structure, placement, library, network, rules,
and wiring content remain KiCad-authored. It is the differential fixture for
the optional KiCad 10 ActionPlugin bridge.

## `add_board_text` coordinates (#838)

`board_text_coordinate_tests` in `pcb_board.rs` adds a `gr_text` to a copy of
this board through the served `add_board_text` file path, with float noise of
the kind older responses reported (#747). KiCad is the oracle: the board the
unfixed build wrote, resaved by KiCad 10.0.6 with
`kicad-cli pcb upgrade --force`, holds

| Argument | Unfixed build wrote | KiCad's resave |
|---|---|---|
| `x` `54.60999999999999`, `y` `27.939999999999998`, `rotation` `90.00000000000001` | `(at 54.60999999999999 27.939999999999998 90.00000000000001)` | `(at 54.61 27.94 90)` |
