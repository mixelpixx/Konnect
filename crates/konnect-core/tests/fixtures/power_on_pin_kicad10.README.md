# Power-symbol-on-pin fixtures

`power_on_pin_kicad10.kicad_sch` carries pins facing each way, a mirrored
symbol, a power unit placed apart from its amplifier, a De Morgan gate, and a
reference two symbols share. `add_power_symbol` with `reference` + `pin_number` is checked
against it (#727). `power_on_off_grid_pin.kicad_sch` supplies a pin off the
1.27mm grid.

## Provenance

`power_on_pin_kicad10.kicad_sch` was built through Konnect against KiCad's
stock `Device`, `Amplifier_Operational`, `74xx` and `power` libraries: `create_project`,
`add_schematic_component`, and two coordinate `add_power_symbol` calls that
put `#PWR01` (GND) and `#PWR02` (+3V3) clear of every pin, so the sheet embeds
KiCad's own `power:GND` and `power:+3V3`. It was then parsed and force-resaved
by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force power_on_pin_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`. The instance records name
project `power_on_pin_kicad10`, so tests write the sheet under that name.

`power_on_off_grid_pin.kicad_sch` is KiCad 10.0.6's
`demos/simulation/power_supplies/hv_converter/hv_converter.kicad_sch`, copied
byte for byte. Its instance records name project `HSCConverter4_load`. No
Konnect placer can make an off-grid pin, because they all snap, so the off-grid
case comes from a file KiCad itself ships.

## Cases

| Symbol | Placement | Pins |
|---|---|---|
| `R1` `Device:R` | `(100.33, 80.01)` | pin 1 up, pin 2 down |
| `R2` `Device:R` | rotated 90 | pin 1 left, pin 2 right |
| `R3` `Device:R` | mirrored about x | pin 1 *below*, pin 2 above |
| `U1` `LM358` | unit 1 at `(100.33, 120.65)`, unit 3 at `(139.7, 120.65)` | pins 1, 2, 3 on unit 1; 8 and 4 on unit 3 only |
| `R4` `Device:R` | twice, at `(180.34, 120.65)` and `(200.66, 120.65)` | each copy has its own pin 1 |
| `U2` `74LS00` | unit 1 at `(241.3, 120.65)` | pin 3 right; drawn by both body styles |

## KiCad's own answer

### Where the pins are

```text
kicad-cli sch erc --severity-all power_on_pin_kicad10.kicad_sch
```

ERC lists every unconnected pin at its own coordinates. Those are the points
the tests expect:

| Pin | KiCad's position |
|---|---|
| R1.1 | `(100.33, 76.20)` |
| R1.2 | `(100.33, 83.82)` |
| R2.1 | `(127.00, 80.01)` |
| R2.2 | `(134.62, 80.01)` |
| R3.1 | `(160.02, 83.82)` |
| R3.2 | `(160.02, 76.20)` |
| U1.1 | `(107.95, 120.65)` |
| U1.3 | `(92.71, 118.11)` |
| U1.8 | `(137.16, 113.03)` |
| U1.4 | `(137.16, 128.27)` |
| R4.1 | `(180.34, 116.84)` and `(200.66, 116.84)` |
| U2.3 | `(248.92, 120.65)` |

R4 is reported once per copy, and the netlist keeps the two copies as separate
nets (`unconnected-(R4-Pad1)` and `unconnected-(R4-Pad1)_1`). A request for
"R4 pin 1" therefore names two places.

`74LS00` carries a De Morgan body style. Its library draws pin 3 in both
`74LS00_1_1` and `74LS00_1_2`, at the same point, and KiCad reports U2.3 once.
Counting the drawings as two pins would wrongly refuse U2.3 as ambiguous.

One test edits the sheet in memory: it renames the second R4's `lib_id` to one
the sheet does not embed. The edited text is never committed. It checks that a
copy whose definition is missing is refused, not skipped in favour of the
other copy.

### Which way a rail faces

Each stock power symbol's single pin sits at its origin. The pin's angle points
into the body, as any KiCad pin's does. All 101 symbols in KiCad 10.0.6's
`power.kicad_sym` follow this: GND's pin is `270` with its graphic below the
origin, and +3V3's is `90` with its graphic above. A rail faces away from a
pin whose outward direction is `D` when it is rotated by `D` minus its own pin
angle:

| Pin | Outward | GND rotation | +3V3 rotation |
|---|---|---|---|
| R1.2, R3.1, U1.4 | down `270` | `0` | |
| R1.1, U1.8 | up `90` | | `0` |
| R2.2, U1.1, U2.3 | right `0` | `90` | `270` |
| R2.1, U1.3 | left `180` | `270` | `90` |

KiCad's demo projects agree. 268 stock `power:` symbols sit directly on a
component pin across the 10.0.6 demos. All 204 on vertical pins face away from
the body. Of the 68 on horizontal pins, 44 are turned to face away and 24 keep
pointing down. That split is why `rotation` still wins when given.

### What placing them produces

With every row above placed through the debug build, KiCad resolves:

```text
kicad-cli sch export netlist power_on_pin_kicad10.kicad_sch
```

| Net | Pins |
|---|---|
| `+3V3` | R1.1, U1.1, U1.3, U1.8 |
| `GND` | R1.2, R2.1, R2.2, R3.1, U1.4, U2.3 |
| `unconnected-(R3-Pad2)` | R3.2 |

R3.1 on `GND` with R3.2 left alone is the row a mirror-blind implementation
cannot produce: unmirrored, pin 1 sits at the top. `kicad-cli sch export svg`
of the same file shows each rail pointing away from its symbol.

### The off-grid pin

`power_on_off_grid_pin.kicad_sch` draws `C4` with pin 2 at `(149.352, 84.328)`,
off the 1.27mm grid on both axes. KiCad's netlist of the demo puts C4.2 on
`/d4` with C6.1, D4.2 and D5.1. With a GND placed through the debug build:

| Placement | KiCad's netlist |
|---|---|
| `reference: C4, pin_number: 2`, lands on `(149.352, 84.328)` | `/d4` merges into `GND` |
| `x: 149.352, y: 84.328`, snaps to `(149.86, 83.82)` | `/d4` unchanged, the symbol touches nothing |

D2 pin 2 looks like a simpler choice, but it is not usable here: its snapped
point falls on D2's own wire, so both placements join it to GND.
