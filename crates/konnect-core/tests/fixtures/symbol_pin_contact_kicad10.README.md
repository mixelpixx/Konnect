# Pin-tip contact fixture

`symbol_pin_contact_kicad10.kicad_sch` puts power symbols on resistor and
regulator pins, so `check_schematic_overlaps` can be checked for telling a pin-tip
connection apart from a collision (#745).

## Provenance

Built through Konnect against KiCad's stock `Device`, `Regulator_Linear` and
`power` libraries with `create_schematic`, `add_schematic_component` and
`add_power_symbol`, then parsed and force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force symbol_pin_contact_kicad10.kicad_sch
```

`add_power_symbol` snaps to the 1.27 mm grid, so a point partway along a
1.27 mm resistor pin lands on its tip. A first attempt put `GND` on R15 pin 1
that way and KiCad netted it to the tip. The off-tip case uses U1's 2.54 mm VIN
pin instead, whose midpoint is on the grid.

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, KiCad's own library records and UUIDs.

## Cases

| Pair | Placement | Expected |
|---|---|---|
| `R10` / `#PWR01` | `+3V3` on R10 pin 1's tip `(273.05, 91.44)`, R10 rotated 270 | contact — the filed case |
| `U1` / `#PWR04` | `+3V3` on AP2112K-3.3 VOUT's tip `(208.28, 88.9)` | contact — the issue's second instance |
| `R11` / `#PWR02` | `GND` on vertical R11 pin 2's tip `(100.33, 64.77)` | contact — the issue's predicted control |
| `R12` / `R13` | two vertical resistors 1.27 mm apart | collision — the bodies share area |
| `R14` / `#PWR03` | `+3V3` at R14 pin 1's root `(160.02, 58.42)` | collision — the `+3V3` drawing covers the pin |
| `U1` / `#PWR05` | `GND` partway along VIN, `(194.31, 88.9)` | collision — a pin tip on another pin's length |

Before the fix, the envelope comparison reported the first two contacts as
`component_overlap`, alongside the three collisions. `R11` / `#PWR02` was not
reported before or after, as the issue predicted.

## KiCad's own answer

KiCad has no symbol-overlap check, so the oracle is which pins KiCad connects:

```text
kicad-cli sch export netlist --output symbol_pin_contact.net symbol_pin_contact_kicad10.kicad_sch
```

| Net | Pins KiCad resolves | What it corroborates |
|---|---|---|
| `+3V3` | R10.1, U1.5 | both `+3V3` pins sit exactly on those pin tips |
| `GND` | R11.2 | the `GND` pin sits on R11 pin 2's tip |
| `unconnected-(R14-Pad1)` | R14.1 | `#PWR03` touches R14's pin without reaching its tip |
| `unconnected-(U1-VIN-Pad1)` | U1.1 | `#PWR05`'s pin lies on VIN's length, not its tip |

The last two rows are the ones a wrong implementation could not produce. Each
power symbol touches a pin, but KiCad connects neither, so neither touch is
pin-tip contact. `#PWR05` in particular meets VIN only at its own pin, so a
check that accepts any pin endpoint on a pin would pass it.

## ERC

`kicad-cli sch erc --severity-all` finds 17 violations, all from leaving pins
open on purpose: 13 `pin_not_connected`, 3 `power_pin_not_driven` and 1
`pin_not_driven`. No violation concerns the overlaps.
