# Rigid block move fixture

`rigid_block_move_kicad10.kicad_sch` holds a closed block of circuit beside
items that a rigid `move_region` (`include: "all"`) must either leave alone or
refuse to cut (#896). `rigid_block_move_child_kicad10.kicad_sch` is the blank
child of its one hierarchical sheet, holding only the `SP` hierarchical label
that its sheet pin names.

## Provenance

Built through Konnect against KiCad's stock `Device` and `power` libraries:
`create_schematic`, `add_schematic_component`, `add_wire` (which inserted both
junctions at the T points), `add_schematic_net_label`, `add_power_symbol`,
`add_no_connect`, `add_bus`, `add_bus_entry`, `add_hierarchical_sheet` and
`add_sheet_pin`. Both files were then parsed and force-resaved by KiCad 10.0.6
on Fedora:

```text
kicad-cli sch upgrade --force rigid_block_move_kicad10.kicad_sch
kicad-cli sch upgrade --force rigid_block_move_child_kicad10.kicad_sch
```

What is committed is that Eeschema serialization, with edits made before the
last resaves, because Konnect has no tool for these items. Typed in by hand:

- a `(property "Netclass" "Power" (at 71.12 49.53 0) …)` field on the `SIG` label;
- a `netclass_flag` directive label at `(66.04, 69.85)` on the bottom rail,
  UUID `5e6a0c1d-…5f60`, with its own `Netclass` field at `(66.7385, 67.31)`;
- a `text_box` "Block" at `(52.07, 41.91)`, 7.62 x 5.08, UUID `5e6a0c1d-…5f61`.

Each time KiCad kept the item, added `(show_name no)` and
`(do_not_autoplace no)` to the fields, and changed nothing else. KiCad itself wrote the `OUT` global label's
`Intersheetrefs` field on the label's anchor.

## Layout

| Item | Where | Role |
|---|---|---|
| the block, box `(50, 40)-(110, 92)` | `R1`, `C1`, `R2` at y 60.96, `#PWR01` (GND) at `(71.12, 72.39)` | moved as one piece |
| 10 wires, 2 junctions | inside the box | top rail at y 52.07, bottom rail at y 69.85 |
| `SIG` label, `OUT` global label | `(71.12, 52.07)`, `(101.6, 52.07)` | name the top rail; `SIG` carries a `Netclass` field at `(71.12, 49.53)` |
| directive label, text box | `(66.04, 69.85)`, `(52.07, 41.91)` | kept by Konnect's model only as raw nodes |
| bus `25b38bed`, bus entry `4b0ba710` | bus `(55.88, 83.82)-(101.6, 83.82)` | with labels `D[0..3]` and `D0` |
| no-connect `f607abfb` | `(93.98, 64.77)`, on `R2` pin 2 | follows its pin |
| `R3` | `(139.7, 60.96)` | joins the top rail by its own `SIG` label |
| wire `f061bf4b` | `(139.7, 69.85)-(160.02, 69.85)` | crosses the box `(120, 40)-(150, 92)` |
| `#PWR02` (GND) | `(160.02, 72.39)` | puts `R3` pin 2 on GND |
| `R4` | `(63.5, 114.3)` | pin 1 at the end of the dangling stub `(63.5, 105.41)-(63.5, 110.49)` |
| no-connect `fab384f8` | `(63.5, 118.11)`, on `R4` pin 2 | in the box `(50, 116)-(80, 125)`, which holds R4 pin 2 but not R4's origin |
| sheet `a9a16ec1` | `(121.92, 99.06)`, 25.4 x 15.24 | crosses `(110, 95)-(130, 130)`, inside `(115, 95)-(170, 120)` |
| sheet pin `SP`, wire `740ed2d5`, label `SP_NET` | pin `(147.32, 104.14)`, wire to `(167.64, 104.14)`, label at `(160.02, 104.14)` | a net with no symbol pin; the wire passes through `(155, 100)-(165, 108)` with both ends outside |

## Oracle

`kicad-cli sch export netlist` on the fixture:

| Net | Pins |
|---|---|
| `GND` | C1.2, R1.2, R3.2 |
| `OUT` | C1.1, R1.1, R2.1, R3.1 |
| `unconnected-(R2-Pad2)` | R2.2 |
| `unconnected-(R4-Pad1)` | R4.1 |
| `unconnected-(R4-Pad2)` | R4.2 |

The `SP_NET` net has no symbol pin, so the netlist does not list it.

`kicad-cli sch erc --severity-all` reports `pin_not_connected` (R4 pin 1),
`label_dangling` (`D0`, `SP_NET`, and `SP` in the child, none of which reach a
symbol pin), `undefined_netclass` (`Power` on `SIG` and on the directive
label, as there is no project file),
`power_pin_not_driven` (`#PWR01`), `unconnected_wire_endpoint` (the stub at
`(63.5, 105.41)` and the `SP` wire's free end) and `multiple_net_names` (`OUT`
and `SIG`). All are deliberate.

| Move | KiCad's answer | Expected |
|---|---|---|
| the block by `(-12.7, -12.7)` | the same five nets in the netlist's `(nets …)` section, byte for byte; the same five ERC violations, each shifted by `(-12.7, -12.7)` | moves, every item by exactly the offset, the fields of `SIG` and the directive label with them |
| the block by `(0, +40.64)` | `R1` pin 2 lands on the stub end, and `GND` becomes C1.2, R1.2, R3.2, R4.1 | refused: R4 pin 1 would join GND |
| box `(120, 40)-(150, 92)` | — | refused: wire `f061bf4b` crosses the edge |
| box `(50, 75)-(80, 92)` | — | refused: the bus crosses the edge |
| box `(110, 95)-(130, 130)` | — | refused: the sheet crosses the edge |
| box `(50, 116)-(80, 125)` by `(2.54, 0)` | — | refused: R4 crosses the edge |
| box `(115, 95)-(170, 120)` by `(2.54, 0)` | — | the sheet, its pin, the wire and `SP_NET` move together |
| box `(155, 100)-(165, 108)` by `(0, -2.54)` | — | refused: wire `740ed2d5` passes through the box |
| the block by `(+99.06, +27.94)` | — | refused: the `D0` stub lands on the `SP` wire's free end, and `SP_NET` would become `D0` |
| the block by `(-30.48, +53.34)`, R2 and R4 renamed `R?`, no-connects removed | — | refused: R2 pin 2 stacks on R4 pin 2 |

The `(0, +40.64)` netlist was taken from a file written with the net check
disabled; the shipped tool refuses it.
