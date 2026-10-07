# Power-row fixture

`power_rows_kicad10.kicad_sch` holds rows of pins that `batch_add_power_symbol`
should join under one power symbol, and pins it must leave apart (#856).
The tests in `sch_wiring.rs::batch_power_tests` run against it.

## Provenance

The sheet was built through Konnect against KiCad's stock `MCU_ST_STM32F1`,
`Device` and `power` libraries. The calls were `create_project`,
`batch_place_components`, one `add_schematic_net_label` (`BLOCK`), and two
coordinate `add_power_symbol` calls. Those put `#PWR01` (GND) at
`(50.8, 180.34)` and `#PWR02` (+3V3) at `(63.5, 180.34)`, clear of every pin,
so the sheet embeds KiCad's own `power:GND` and `power:+3V3`. KiCad 10.0.6 then
parsed and force-resaved it:

```text
kicad-cli sch upgrade --force power_rows_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`. The sheet has no wires. Its
instance records name project `power_rows_kicad10`, so tests write it under
that name.

## Cases

| Symbol | Placement | Role |
|---|---|---|
| `U1` `STM32F103C8Tx` | `(101.6, 88.9)` | VSS 23/35/47 stacked on one point, VSSA 8 beside them; VBAT 1, VDD 24/36/48 and VDDA 9 in a row on top |
| `C1`, `C2`, `C3` `Device:C` | `(139.7 / 147.32 / 154.94, 139.7)` | a row of decoupling caps, 7.62 mm apart |
| `C4` `Device:C` | `(175.26, 139.7)` | on the same row, 20.32 mm past `C3` |
| `C7` `Device:C` | `(195.58, 139.7)` | pin 2 points down |
| `C8` `Device:C` | `(203.2, 147.32)`, rotated 180 | pin 2 points up, on `C7`'s row 7.62 mm away |
| `R1` `Device:R` | `(139.7, 63.5)`, rotated 90 | pin 1 left, pin 2 right |
| `C5`, `C6` `Device:C` | `(139.7 / 147.32, 170.18)` | a pair whose bar would cross local label `BLOCK` at `(143.51, 176.53)` |

## KiCad's own answer

### Where the pins are

```text
kicad-cli sch erc --severity-all power_rows_kicad10.kicad_sch
```

ERC lists every unconnected pin at its own coordinates. These are the pins the
tests name:

| Pin | KiCad's position |
|---|---|
| U1.23 (VSS) | `(101.60, 129.54)`, reported once for 23, 35 and 47 |
| U1.8 (VSSA) | `(104.14, 129.54)` |
| U1.1, U1.24, U1.36, U1.48, U1.9 | `(96.52 / 99.06 / 101.60 / 104.14 / 106.68, 48.26)` |
| C1.2, C2.2, C3.2, C4.2 | `(139.70 / 147.32 / 154.94 / 175.26, 143.51)` |
| C1.1, C2.1, C3.1, C4.1 | the same x, `135.89` |
| C7.2 | `(195.58, 143.51)` |
| C8.2 | `(203.20, 143.51)` |
| R1.1, R1.2 | `(135.89, 63.50)`, `(143.51, 63.50)` |
| C5.2, C6.2 | `(139.70, 173.99)`, `(147.32, 173.99)` |

ERC on the fixture as committed reports 66 `pin_not_connected`, 9
`power_pin_not_driven`, 2 `pin_not_driven` (NRST and BOOT0) and 1
`label_dangling` (`BLOCK`).

### What the tool's result connects

These are the tests' calls, run on a copy with the branch's
`target/debug/konnect` over stdio:

1. `batch_add_power_symbol` with GND on U1.23, U1.35, U1.47, U1.8, C1.2–C4.2,
   C7.2, C8.2, R1.1, R1.2, C5.2 and C6.2.
2. `batch_add_power_symbol` with +3V3 on U1.1, U1.24, U1.36, U1.48, U1.9 and
   C1.1–C4.1.

The result was then checked with KiCad:

```text
kicad-cli sch export netlist power_rows_kicad10.kicad_sch
kicad-cli sch erc --severity-all power_rows_kicad10.kicad_sch
```

| Net | Pins in KiCad's netlist |
|---|---|
| `GND` | C1.2 C2.2 C3.2 C4.2 C5.2 C6.2 C7.2 C8.2 R1.1 R1.2 U1.23 U1.35 U1.47 U1.8 |
| `+3V3` | C1.1 C2.1 C3.1 C4.1 U1.1 U1.24 U1.36 U1.48 U1.9 |

Every pin named in a call is on its rail. Afterwards ERC no longer lists any of
them as unconnected. `BLOCK` is still `label_dangling`, so the bar that would
have crossed it was not drawn: had it been, KiCad would have joined `BLOCK` to
GND. `power_pin_not_driven` drops to one per rail, on `#PWR01` and `#PWR02`,
because the sheet has no `PWR_FLAG`.

The groups the tests expect:

| Pins | Joined | Symbol at | Rotation |
|---|---|---|---|
| U1.23, U1.35, U1.47, U1.8 | yes | `(101.6, 132.08)` | 0 |
| C1.2, C2.2, C3.2 | yes, dot at `(147.32, 146.05)` | `(147.32, 146.05)` | 0 |
| C4.2 | no: 20.32 mm > `max_gap` | `(175.26, 143.51)` | 0 |
| C7.2 | no: faces away from C8.2 | `(195.58, 143.51)` | 0 |
| C8.2 | no | `(203.2, 143.51)` | 180 |
| R1.1 / R1.2 | no: opposite ends of one part | `(135.89, 63.5)` / `(143.51, 63.5)` | 270 / 90 |
| C5.2 / C6.2 | no: bar would touch `BLOCK` | `(139.7, 173.99)` / `(147.32, 173.99)` | 0 |
| U1.1, U1.24, U1.36, U1.48, U1.9 | yes, dots at x = 99.06, 101.6, 104.14 | `(101.6, 45.72)` | 0 |

Each joined row has a stub from every pin to 2.54 mm beyond it, and one bar
along the stub ends. The symbol sits on the end of the middle stub (for an even
count, the one just before the middle).
