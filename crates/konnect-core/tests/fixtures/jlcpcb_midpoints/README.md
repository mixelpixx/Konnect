# Native midpoint regression fixture

Captured on Windows with KiCad 10.0.6, 2026-10-01, through the public IPC API.
The board started from KiCad's `gr_poly_outline.kicad_pcb` fixture. Native
`CreateItems` placed the installed official
`Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical` on F.Cu and B.Cu
at 0°, 90°, 180°, 270°, and 45°. Front/back pairs deliberately overlap;
this is a geometry regression fixture, not a manufacturable design.
The official `Resistor_SMD:R_0805_2012Metric` at (80,20) is the centre-anchored
control; its native position strings remain `80.000000,-20.000000` in CPL.

`midpoints.kicad_pcb` is native `SaveDocumentToString` output. `footprints.pb`
contains native `GetItems` footprint messages. `boxes.pb` records native
`GetBoundingBox` pad boxes, paired by UUID, independently of the midpoint
algorithm. Tests replay these responses through NNG with exact target assertions.

For JF/JB0–4, literal board-space union centres in millimetres are:

| Rotation | Anchor | Pad-box union centre |
| --- | --- | --- |
| 0° | (20,20) | (20,21.27) |
| 90° | (30,20) | (31.27,20) |
| 180° | (40,20) | (40,18.73) |
| 270° | (50,20) | (48.73,20) |
| 45° | (60,20) | (60.721984,20.721984) |

The 45° case combines a rectangular and circular pad, distinguishing the
board-space union from rotation of a local centre. CPL coordinates invert Y
once, preserve X on both sides, and retain the existing rotation policy.

To reproduce: open a disposable copy of the board, enable IPC, set
`KONNECT_MIDPOINT_BOARD` to that copy's absolute path and run:

```text
cargo test -p konnect-core --locked --lib live_native_midpoint_acceptance -- --ignored --nocapture
```

The acceptance test reads the live board and exports through real `kicad-cli`.
`KICAD_CLI_PATH` overrides the CLI executable. Fixture generation is a separate
ignored test (`live_native_midpoint_fixture`) for a disposable empty board and
the official installed footprint given by `KONNECT_MIDPOINT_LIBRARY`. Setting
`KONNECT_MIDPOINT_CENTERED_LIBRARY` names the installed 0805 footprint for that
generator. Setting
`KONNECT_MIDPOINT_CAPTURE_DIR` during acceptance records the native artifacts;
ordinary CI never regenerates the oracle.

The position-exclusion regression replays these native messages with targeted
in-memory changes to two footprints' attributes and reference fields. Both share
the first included footprint's reference. Variants cover excluded footprints
with/without pads, DNP, actual exported duplicates, and BOM-only exclusion. The
captured geometry is unchanged; these variants are controlled test mutations,
not additional live KiCad captures. The included footprint must retain its
original observed midpoint and convert its position row successfully.
