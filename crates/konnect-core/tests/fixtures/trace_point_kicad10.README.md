# Point-tracing fixture

`trace_point_kicad10.kicad_sch` carries one of each thing that can sit on a
schematic point, so `trace_from_point` can be asked what is there and checked
against an answer that is not its own (#539).

## Provenance

Built through Konnect against KiCad's stock `Device` and
`Amplifier_Operational` libraries — `create_schematic`,
`add_schematic_component`, `connect_to_net`, `add_wire`, `add_junction` — then
parsed and force-resaved by KiCad 10.0.6:

```text
kicad-cli sch upgrade --force trace_point_kicad10.kicad_sch
```

What is committed is that Eeschema serialization: `(generator "eeschema")`,
`(version 20260306)`, `(generator_version "10.0")`, KiCad's own library
records, field positions and UUIDs.

Only two junction dots are in the file, and both are deliberate. A first
attempt carried a third: `add_wire` already inserts a dot where a branch lands
mid-span, so the explicit `add_junction` at `(130.81, 121.92)` added a second
one on top of it. The redundant call was dropped rather than documented.

## Cases

| Point | What is there | Why |
|---|---|---|
| `(100.33, 96.52)` | `R1` pin 1, two wire ends, a dot, net `NETA` | the filed repro — all four kinds at once |
| `(100.33, 104.14)` | `R1` pin 2, one wire end | a pin must not imply a dot |
| `(130.81, 121.92)` | a dot, two wires | a dot must not imply a pin |
| `(160.02, 104.14)` | `R2` pin 2 and `R3` pin 1, no wire | two pins stacked on one point |
| `(198.12, 130.81)` | `U1` pin 7, unit 2 only | unit 1 is placed 30mm away (#35) |
| `(210, 50)` | nothing | every list present and empty |

`R2` and `R3` touch pin-to-pin with no wire between them, which is why KiCad's
netlist is the load-bearing evidence below: nothing in the file says those two
pins are connected except their coordinates.

## KiCad's own answer

The pins reached by each net are KiCad's, not this crate's:

```text
kicad-cli sch export netlist --output trace_point.net trace_point_kicad10.kicad_sch
```

| Net | Pins KiCad resolves | What it corroborates |
|---|---|---|
| `/NETA` | 1 — R1.1 | the pin really is on the point the label's net reaches |
| `Net-(R2-Pad2)` | 2 — R2.2, R3.1 | the two stacked pins really do coincide |
| `unconnected-(R1-Pad2)` | 1 — R1.2 | the wire off pin 2 reaches nothing else |
| `unconnected-(U1-Pad1)` | 1 — U1.1 | unit 1's output |
| `unconnected-(U1-Pad7)` | 1 — U1.7 | unit 2's output, a separate net, so a separate point |

`Net-(R2-Pad2)` and the split between `U1-Pad1` and `U1-Pad7` are the two rows
a wrong implementation could not produce: the first says two pins share a
coordinate, the second says two units do not.

The tool reports `net: null` at `(160.02, 104.14)`. That is not a disagreement
with the netlist — `Net-(R2-Pad2)` is a name the exporter invents for an
unnamed net, and no label on the sheet names it.

## ERC

`kicad-cli sch erc --severity-all` finds 22 violations, all of them the cost of
a sheet built out of deliberate stubs rather than defects in the fixture: 9
`pin_not_connected`, 5 `unconnected_wire_endpoint`, 4 `pin_not_driven`, 1
`wire_dangling`, 1 `isolated_pin_label` on `NETA`, and — because the LM358's
power unit 3 is not placed — 1 `missing_power_pin` and 1 `missing_unit`.

No short is reported between `R2` and `R3`, which is the intended reading of
two pins on one point.
